use std::{future::Future, sync::atomic::Ordering};
#[cfg(test)]
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use ahash::AHashMap;
use app_config::network::{
    Config, KytrixServiceCapability, NetworkRouteGeneration, ProxyRoute, ResolvedProxy,
    RouteTarget, ServiceTarget,
};
#[cfg(test)]
use app_config_provider::notification::ConfigNotification;
use bytes::Bytes;
use kytrix_proto::{
    client::{
        Reauthenticate, Request as ClientRequest, SetSubscriptions,
        request::Request as ClientRequestKind,
    },
    server as proto_response,
};
use network::transport::{error::TransportError, websocket::RawWebSocket};
use prost::Message;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
#[cfg(test)]
use tokio::sync::{Notify, oneshot, watch};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use url::Url;
#[cfg(test)]
use widget_tokio_runtime::runtime::Runtime;
use yawc::{Frame, OpCode};

#[cfg(test)]
use crate::auth::test_support::{test_transport, test_transport_policy};
use crate::{
    account_profile::AccountProfile,
    auth::model::SessionAvailability,
    service::{
        engine::{
            BeginRpcRequest, Command, ConnectedAction, DisconnectedAction, ErrorCode,
            FrameDisposition, INITIAL_RECONNECT_DELAY, IncomingMessage, KytrixServiceError,
            KytrixServiceEvent, KytrixSessionState, LocalSubscriber, MAX_PENDING_RPCS,
            MAX_RECONNECT_DELAY, OPERATION_TIMEOUT, PONG_TIMEOUT, PendingRenewal, PendingRpc,
            RENEWAL_RESPONSE_GRACE, RENEWAL_RETRY_INTERVAL, RenewalAttemptError, RenewalChallenge,
            RenewalFuture, RenewalPoll, RenewalStage, SUBSCRIBER_SWEEP_INTERVAL,
            ServiceSessionActor, ServiceSessionContext, ServiceTopic, SubscriberSink,
            SubscriptionRouteFence, WRITE_TIMEOUT, authorization_url, normalize_topics,
            poll_renewal, renewal_pollable, service_authorization_headers_for,
            service_authorization_headers_for_renewal, service_authorization_headers_with_profile,
            service_unix_deadline, service_unix_now, subscription_transport_error,
            valid_protocol_identifier,
        },
        protocol::{
            decode_binary, flush_protocol_frames, heartbeat_delay, next_server_message,
            send_frame_with_timeout, validate_response, validate_rpc_response,
        },
    },
};
#[cfg(test)]
use crate::{
    auth::{model::SessionChange, session::AuthSession},
    service::engine::{
        CancelOnDrop, HEARTBEAT_MAX_SECONDS, HEARTBEAT_MIN_SECONDS, IndexEvent,
        KYTRIX_SERVICE_WS_URL, NewsEvent, NewsKind, SessionStateRegistry,
    },
};

impl ServiceSessionActor {
    #[cfg(test)]
    pub(super) fn new(
        commands: mpsc::Receiver<Command>,
        cancellation: CancellationToken,
        wake: Arc<Notify>,
    ) -> Self {
        let auth = AuthSession::for_test(test_transport());
        let credential_invalidations = auth.session_changes();
        let session_state = SessionStateRegistry::new();
        let config = ConfigNotification::new(Arc::new(Config::default()));
        Self::from_context(
            commands,
            ServiceSessionContext {
                websocket_url: KYTRIX_SERVICE_WS_URL.clone(),
                config,
                auth,
                account_profile: Arc::new(RwLock::new(None)),
                credential_invalidations,
                session_state,
                cancellation,
                wake,
                transport_policy: test_transport_policy(),
                runtime: Runtime::for_test(),
            },
            ResolvedProxy::NoProxy,
            0,
            1,
        )
    }

    #[cfg(test)]
    pub(super) fn with_url(
        commands: mpsc::Receiver<Command>,
        cancellation: CancellationToken,
        wake: Arc<Notify>,
        websocket_url: Url,
        account_profile: Arc<RwLock<Option<AccountProfile>>>,
        credential_invalidations: watch::Receiver<SessionChange>,
    ) -> Self {
        let session_state = SessionStateRegistry::new();
        let config = ConfigNotification::new(Arc::new(Config::default()));
        let auth = AuthSession::for_test(test_transport());
        Self::from_context(
            commands,
            ServiceSessionContext {
                websocket_url,
                config,
                auth,
                account_profile,
                credential_invalidations,
                session_state,
                cancellation,
                wake,
                transport_policy: test_transport_policy(),
                runtime: Runtime::for_test(),
            },
            ResolvedProxy::NoProxy,
            0,
            1,
        )
    }

    pub(super) fn from_context(
        commands: mpsc::Receiver<Command>,
        context: ServiceSessionContext,
        proxy: ResolvedProxy,
        os_proxy_fingerprint: u64,
        session_generation: u64,
    ) -> Self {
        let ServiceSessionContext {
            websocket_url,
            config,
            auth,
            account_profile,
            credential_invalidations,
            session_state: observed_session_state,
            cancellation,
            wake,
            transport_policy,
            runtime,
        } = context;
        let committed_credential_change = *credential_invalidations.borrow();
        Self {
            commands,
            cancellation,
            wake,
            websocket_url,
            proxy,
            os_proxy_fingerprint,
            config,
            auth,

            next_request_id: 1,
            pending_rpcs: AHashMap::with_capacity(MAX_PENDING_RPCS),
            subscription_route_change_reuses_socket: false,
            subscribers: AHashMap::with_capacity(8),
            topics: AHashMap::with_capacity(4),
            latest_indices: AHashMap::with_capacity(8),
            latest_ais_keyframes: AHashMap::with_capacity(2),
            session_state: None,
            subscriptions_dirty: false,
            websocket: None,
            heartbeat_deadline: Instant::now() + heartbeat_delay(),
            sweep_deadline: Instant::now() + SUBSCRIBER_SWEEP_INTERVAL,
            pending_pong: None,
            renewal: None,
            account_profile,
            highest_credential_revision: 0,
            credential_invalidations,
            credential_events_open: true,
            committed_credential_change,
            observed_session_state,
            session_generation,
            transport_policy,
            runtime,
        }
    }

    pub(super) async fn run(mut self) {
        let mut reconnect_delay = INITIAL_RECONNECT_DELAY;
        let mut reconnect_deadline = None;
        let mut system_proxy_refresh = None::<JoinHandle<()>>;
        let mut reconnect_after_system_proxy_refresh = false;
        loop {
            self.prune_subscribers();
            if self.cancellation.is_cancelled() {
                break;
            }
            if self.subscribers.is_empty()
                && self.pending_rpcs.is_empty()
                && self.websocket.is_some()
            {
                if let Some(mut websocket) = self.websocket.take() {
                    let _ = self.flush_subscriptions(&mut websocket).await;
                    let _ = websocket.close().await;
                }
                continue;
            }

            if self.websocket.is_none() {
                if !self.subscribers.is_empty() {
                    for subscriber in self.subscribers.values() {
                        subscriber.sink.reconnecting();
                    }
                    let deadline =
                        *reconnect_deadline.get_or_insert_with(|| Instant::now() + reconnect_delay);
                    let retry = sleep_until(deadline);
                    tokio::pin!(retry);
                    let action = tokio::select! {
                        biased;
                        () = self.cancellation.cancelled() => break,
                        _ = async {
                            if let Some(task) = system_proxy_refresh.as_mut() {
                                let _ = task.await;
                            }
                        }, if system_proxy_refresh.is_some() => {
                            system_proxy_refresh.take();
                            Some(DisconnectedAction::SystemProxyRefreshCompleted)
                        },
                        () = self.wake.notified() => {
                            self.start_system_proxy_refresh(&mut system_proxy_refresh);
                            self.prune_pending_rpcs();
                            None
                        },
                        changed = self.credential_invalidations.changed(), if self.credential_events_open => {
                            self.commit_credential_notification(changed.is_ok());
                            None
                        }
                        command = self.commands.recv() => {
                            let Some(command) = command else { break };
                            if self.handle_disconnected_command(command).await {
                                reconnect_delay = INITIAL_RECONNECT_DELAY;
                                reconnect_deadline = None;
                                self.reset_heartbeat();
                            }
                            None
                        }
                        () = &mut retry, if !reconnect_after_system_proxy_refresh => {
                            Some(DisconnectedAction::Retry)
                        },
                    };
                    let should_reconnect = match action {
                        Some(DisconnectedAction::Retry) => {
                            self.start_system_proxy_refresh(&mut system_proxy_refresh);
                            self.prune_pending_rpcs();
                            if system_proxy_refresh.is_some() {
                                reconnect_after_system_proxy_refresh = true;
                                false
                            } else {
                                true
                            }
                        }
                        Some(DisconnectedAction::SystemProxyRefreshCompleted) => {
                            self.finish_system_proxy_refresh();
                            self.prune_pending_rpcs();
                            let should_reconnect =
                                reconnect_after_system_proxy_refresh || Instant::now() >= deadline;
                            reconnect_after_system_proxy_refresh = false;
                            should_reconnect
                        }
                        None => false,
                    };
                    if should_reconnect && !self.subscribers.is_empty() {
                        match self.reconnect_subscribers().await {
                            Ok(()) => {
                                reconnect_delay = INITIAL_RECONNECT_DELAY;
                                reconnect_deadline = None;
                            }
                            Err(error) => {
                                self.prepare_reconnect(error);
                                reconnect_delay =
                                    reconnect_delay.saturating_mul(2).min(MAX_RECONNECT_DELAY);
                                reconnect_deadline = Some(Instant::now() + reconnect_delay);
                            }
                        }
                    }
                    continue;
                }
                let command = tokio::select! {
                    biased;
                    () = self.cancellation.cancelled() => break,
                    _ = async {
                        if let Some(task) = system_proxy_refresh.as_mut() {
                            let _ = task.await;
                        }
                    }, if system_proxy_refresh.is_some() => {
                        system_proxy_refresh.take();
                        self.finish_system_proxy_refresh();
                        reconnect_after_system_proxy_refresh = false;
                        continue;
                    },
                    () = self.wake.notified() => continue,
                    changed = self.credential_invalidations.changed(), if self.credential_events_open => {
                        self.commit_credential_notification(changed.is_ok());
                        continue;
                    }
                    command = self.commands.recv() => command,
                };
                let Some(command) = command else { break };
                if self.handle_disconnected_command(command).await {
                    reconnect_delay = INITIAL_RECONNECT_DELAY;
                    reconnect_deadline = None;
                    self.reset_heartbeat();
                }
                continue;
            }

            let mut websocket = self
                .websocket
                .take()
                .unwrap_or_else(|| unreachable!("connected actor must own a WebSocket"));
            if let Err(error) = self.flush_subscriptions(&mut websocket).await {
                self.websocket = None;
                self.prepare_reconnect(error);
                continue;
            }
            let timer = sleep_until(self.heartbeat_deadline);
            let sweep = sleep_until(self.sweep_deadline);
            let can_poll_renewal = renewal_pollable(&self.renewal);
            tokio::pin!(timer);
            tokio::pin!(sweep);
            // The timer must remain fair with continuously arriving frames or commands.
            let incoming = tokio::select! {
                () = self.cancellation.cancelled() => {
                    self.websocket = None;
                    break;
                }
                () = self.wake.notified() => Some(ConnectedAction::Wake),
                _ = async {
                    if let Some(task) = system_proxy_refresh.as_mut() {
                        let _ = task.await;
                    }
                }, if system_proxy_refresh.is_some() => {
                    system_proxy_refresh.take();
                    Some(ConnectedAction::SystemProxyRefreshCompleted)
                },
                changed = self.credential_invalidations.changed(), if self.credential_events_open => {
                    Some(ConnectedAction::CredentialsChanged(changed.is_ok()))
                }
                renewal = poll_renewal(&mut self.renewal), if can_poll_renewal => {
                    Some(ConnectedAction::Renewal(renewal))
                }
                () = &mut timer => Some(ConnectedAction::Heartbeat),
                () = &mut sweep => Some(ConnectedAction::Sweep),
                command = self.commands.recv() => {
                    match command {
                        Some(command) => {
                            let result = self.handle_connected_command(command, &mut websocket).await;
                            Some(ConnectedAction::Command(result))
                        }
                        None => {
                            self.websocket = None;
                            break;
                        }
                    }
                }
                frame = websocket.next_frame() => Some(ConnectedAction::Frame(frame)),
            };

            let Some(action) = incoming else { continue };
            match action {
                ConnectedAction::Command(Ok(())) => {
                    self.websocket = Some(websocket);
                }
                ConnectedAction::Command(Err(error)) => {
                    self.websocket = None;
                    self.prepare_reconnect(error);
                }
                ConnectedAction::Frame(Ok(frame)) => {
                    match self.process_frame(&mut websocket, frame).await {
                        Ok(FrameDisposition::Continue) => {
                            self.websocket = Some(websocket);
                        }
                        Ok(FrameDisposition::CredentialsChanged) => {}
                        Err(error) => {
                            self.websocket = None;
                            self.prepare_reconnect(error);
                        }
                    }
                }
                ConnectedAction::Frame(Err(error)) => {
                    self.websocket = None;
                    let error = KytrixServiceError::Read(smol_str::format_smolstr!("{}", error));
                    self.prepare_reconnect(error);
                }
                ConnectedAction::Heartbeat => {
                    let heartbeat_result = if self.pending_pong.is_some() {
                        Err(KytrixServiceError::Protocol(
                            "Kytrix service Pong timeout".into(),
                        ))
                    } else {
                        let token = rand::random::<u64>().to_be_bytes();
                        let operation_cancellation = self.cancellation.clone();
                        self.await_control_with_routes(
                            &operation_cancellation,
                            Instant::now() + WRITE_TIMEOUT,
                            send_frame_with_timeout(
                                &mut websocket,
                                Frame::ping(Bytes::copy_from_slice(&token)),
                                WRITE_TIMEOUT,
                            ),
                            None,
                            None,
                        )
                        .await
                        .map(|()| {
                            self.pending_pong = Some(token);
                            self.heartbeat_deadline = Instant::now() + PONG_TIMEOUT;
                        })
                    };
                    match heartbeat_result {
                        Ok(()) => self.websocket = Some(websocket),
                        Err(error) => {
                            self.websocket = None;
                            self.prepare_reconnect(error);
                            self.pending_pong = None;
                        }
                    }
                }
                ConnectedAction::Renewal(poll) => {
                    match self.handle_renewal_poll(&mut websocket, poll).await {
                        Ok(()) => self.websocket = Some(websocket),
                        Err(error) => {
                            self.websocket = None;
                            self.prepare_reconnect(error);
                        }
                    }
                }
                ConnectedAction::Sweep | ConnectedAction::Wake => {
                    self.start_system_proxy_refresh(&mut system_proxy_refresh);
                    self.prune_pending_rpcs();
                    self.sweep_deadline = Instant::now() + SUBSCRIBER_SWEEP_INTERVAL;
                    self.websocket = Some(websocket);
                }
                ConnectedAction::SystemProxyRefreshCompleted => {
                    self.finish_system_proxy_refresh();
                    self.prune_pending_rpcs();
                    self.websocket = Some(websocket);
                }
                ConnectedAction::CredentialsChanged(channel_open) => {
                    if self.commit_credential_notification(channel_open) {
                        self.fail_pending_rpcs(KytrixServiceError::AccountChanged);
                        self.renewal = None;
                        self.websocket = None;
                    } else {
                        self.websocket = Some(websocket);
                    }
                }
            }
        }

        self.websocket = None;
        if let Some(task) = system_proxy_refresh.take() {
            task.abort();
            let _ = task.await;
        }
        self.fail_pending_rpcs(KytrixServiceError::Closed);
        for subscriber in self.subscribers.values() {
            subscriber.sink.finish();
        }
        self.subscribers.clear();
        self.topics.clear();
        self.latest_indices.clear();
        self.latest_ais_keyframes.clear();
        self.observed_session_state.clear(self.session_generation);
        self.subscriptions_dirty = false;
        self.renewal = None;
    }

    pub(super) async fn handle_disconnected_command(&mut self, command: Command) -> bool {
        match command {
            Command::Subscribe {
                id,
                topics,
                expected_profile,
                route_generation,
                sink,
                cancellation,
                deadline,
                respond_to,
            } => {
                if cancellation.is_cancelled() {
                    let _ = respond_to.send(Err(KytrixServiceError::Closed));
                    return false;
                }
                if Instant::now() >= deadline {
                    let _ = respond_to.send(Err(KytrixServiceError::Timeout));
                    return false;
                }
                if !self.profile_is_current(expected_profile.as_ref()) {
                    sink.fail(TransportError::CredentialChanged);
                    let _ = respond_to.send(Err(KytrixServiceError::AccountChanged));
                    return false;
                }
                if !self.capability_route_is_current(sink.capability(), route_generation) {
                    sink.fail(TransportError::RouteChanged);
                    let _ = respond_to.send(Err(KytrixServiceError::RouteChanged));
                    return false;
                }
                self.add_subscriber_inner(
                    id,
                    topics,
                    sink,
                    expected_profile.clone(),
                    route_generation,
                );
                if !self.subscribers.contains_key(&id) {
                    let _ = respond_to.send(Err(KytrixServiceError::SlowSubscriber));
                    return false;
                }

                let mut websocket = match self
                    .connect_websocket(deadline, cancellation.clone(), expected_profile.as_ref())
                    .await
                {
                    Ok((websocket, profile)) => {
                        if let Err(error) = self.install_account_profile(profile) {
                            self.remove_subscriber(id);
                            let _ = respond_to.send(Err(error));
                            return false;
                        }
                        websocket
                    }
                    Err(error) => {
                        self.remove_subscriber(id);
                        let _ = respond_to.send(Err(error));
                        return false;
                    }
                };
                let remote_topics = self.remote_topics();
                let Some(route_fence) = self.subscription_route_fence(id) else {
                    let _ = respond_to.send(Err(KytrixServiceError::Closed));
                    return false;
                };
                if let Err(error) = self
                    .send_subscriptions(
                        &mut websocket,
                        remote_topics,
                        deadline,
                        cancellation.clone(),
                        Some(&route_fence),
                    )
                    .await
                {
                    self.remove_subscriber(id);
                    let route_change_reuses_socket =
                        std::mem::take(&mut self.subscription_route_change_reuses_socket);
                    let reuse_socket = matches!(&error, KytrixServiceError::RouteChanged)
                        && route_change_reuses_socket
                        && !self.subscribers.is_empty();
                    let keep_connection =
                        matches!(error, KytrixServiceError::Server(_)) || reuse_socket;
                    let _ = respond_to.send(Err(error));
                    if keep_connection {
                        self.websocket = Some(websocket);
                    }
                    return keep_connection;
                }
                if cancellation.is_cancelled() {
                    self.queue_removed_topics(id);
                    self.websocket = Some(websocket);
                    let _ = respond_to.send(Err(KytrixServiceError::Closed));
                    return true;
                }
                if !self.subscribers.contains_key(&id) {
                    self.websocket = Some(websocket);
                    let _ = respond_to.send(Err(KytrixServiceError::RouteChanged));
                    return true;
                }
                self.websocket = Some(websocket);
                if respond_to.send(Ok(())).is_err() {
                    self.queue_removed_topics(id);
                    return true;
                }
                if let Some(subscriber) = self.subscribers.get(&id) {
                    subscriber.sink.activate();
                }
                true
            }
            Command::Rpc {
                request,
                expected_profile,
                capability,
                route_generation,
                cancellation,
                possibly_submitted,
                deadline,
                respond_to,
            } => {
                if cancellation.is_cancelled() || Instant::now() >= deadline {
                    let _ = respond_to.send(Err(if cancellation.is_cancelled() {
                        KytrixServiceError::Closed
                    } else {
                        KytrixServiceError::Timeout
                    }));
                    return false;
                }
                if !self.profile_is_current(Some(&expected_profile))
                    || !self.capability_route_is_current(capability, route_generation)
                {
                    let _ = respond_to.send(Err(
                        if !self.profile_is_current(Some(&expected_profile)) {
                            KytrixServiceError::AccountChanged
                        } else {
                            KytrixServiceError::RouteChanged
                        },
                    ));
                    return false;
                }
                let (mut websocket, profile) = match self
                    .connect_websocket(deadline, cancellation.clone(), Some(&expected_profile))
                    .await
                {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = respond_to.send(Err(error));
                        return false;
                    }
                };
                if let Err(error) = self.install_account_profile(profile) {
                    let _ = respond_to.send(Err(error));
                    return false;
                }
                if !self.remote_topics().is_empty()
                    && let Err(error) = self
                        .send_subscriptions(
                            &mut websocket,
                            self.remote_topics(),
                            deadline,
                            cancellation.clone(),
                            None,
                        )
                        .await
                {
                    let _ = respond_to.send(Err(error));
                    return false;
                }
                let result = self
                    .begin_rpc_request(
                        &mut websocket,
                        BeginRpcRequest {
                            request,
                            expected_profile,
                            capability,
                            route_generation,
                            deadline,
                            cancellation,
                            possibly_submitted,
                            respond_to,
                        },
                    )
                    .await;
                if result.is_ok() {
                    self.websocket = Some(websocket);
                }
                result.is_ok()
            }
            Command::RetireCredentials => {
                self.retire_subscribers(TransportError::CredentialChanged);
                false
            }
            Command::Reconfigure => {
                let config = self.config.current();
                self.retire_changed_routes(&config);
                false
            }
        }
    }

    pub(super) async fn handle_connected_command(
        &mut self,
        command: Command,
        websocket: &mut RawWebSocket,
    ) -> Result<(), KytrixServiceError> {
        match command {
            Command::Subscribe {
                id,
                topics,
                expected_profile,
                route_generation,
                sink,
                cancellation,
                deadline,
                respond_to,
            } => {
                if cancellation.is_cancelled() {
                    let _ = respond_to.send(Err(KytrixServiceError::Closed));
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    let _ = respond_to.send(Err(KytrixServiceError::Timeout));
                    return Ok(());
                }
                if !self.profile_is_current(expected_profile.as_ref()) {
                    sink.fail(TransportError::CredentialChanged);
                    let _ = respond_to.send(Err(KytrixServiceError::AccountChanged));
                    return Ok(());
                }
                if !self.capability_route_is_current(sink.capability(), route_generation) {
                    sink.fail(TransportError::RouteChanged);
                    let _ = respond_to.send(Err(KytrixServiceError::RouteChanged));
                    return Ok(());
                }
                let new_topics =
                    self.add_subscriber_inner(id, topics, sink, expected_profile, route_generation);
                if !new_topics.is_empty() {
                    let remote_topics = self.remote_topics();
                    let Some(route_fence) = self.subscription_route_fence(id) else {
                        let _ = respond_to.send(Err(KytrixServiceError::Closed));
                        return Ok(());
                    };
                    if let Err(error) = self
                        .send_subscriptions(
                            websocket,
                            remote_topics,
                            deadline,
                            cancellation.clone(),
                            Some(&route_fence),
                        )
                        .await
                    {
                        self.remove_subscriber(id);
                        let route_change_reuses_socket =
                            std::mem::take(&mut self.subscription_route_change_reuses_socket);
                        let reuse_socket = matches!(&error, KytrixServiceError::RouteChanged)
                            && route_change_reuses_socket
                            && !self.subscribers.is_empty();
                        let _ = respond_to.send(Err(error.clone()));
                        return if matches!(error, KytrixServiceError::Server(_)) || reuse_socket {
                            Ok(())
                        } else {
                            Err(error)
                        };
                    }
                }
                if cancellation.is_cancelled() {
                    self.queue_removed_topics(id);
                    let _ = respond_to.send(Err(KytrixServiceError::Closed));
                    return Ok(());
                }
                if !self.subscribers.contains_key(&id) {
                    let _ = respond_to.send(Err(KytrixServiceError::RouteChanged));
                    return Ok(());
                }
                if respond_to.send(Ok(())).is_err() {
                    self.queue_removed_topics(id);
                    return Ok(());
                }
                if let Some(subscriber) = self.subscribers.get(&id) {
                    subscriber.sink.activate();
                }
                Ok(())
            }
            Command::Rpc {
                request,
                expected_profile,
                capability,
                route_generation,
                cancellation,
                possibly_submitted,
                deadline,
                respond_to,
            } => {
                if cancellation.is_cancelled() || Instant::now() >= deadline {
                    let _ = respond_to.send(Err(if cancellation.is_cancelled() {
                        KytrixServiceError::Closed
                    } else {
                        KytrixServiceError::Timeout
                    }));
                    return Ok(());
                }
                if !self.profile_is_current(Some(&expected_profile))
                    || !self.capability_route_is_current(capability, route_generation)
                {
                    let _ = respond_to.send(Err(
                        if !self.profile_is_current(Some(&expected_profile)) {
                            KytrixServiceError::AccountChanged
                        } else {
                            KytrixServiceError::RouteChanged
                        },
                    ));
                    return Ok(());
                }
                self.begin_rpc_request(
                    websocket,
                    BeginRpcRequest {
                        request,
                        expected_profile,
                        capability,
                        route_generation,
                        deadline,
                        cancellation,
                        possibly_submitted,
                        respond_to,
                    },
                )
                .await
            }
            Command::RetireCredentials => {
                self.retire_subscribers(TransportError::CredentialChanged);
                Err(KytrixServiceError::AccountChanged)
            }
            Command::Reconfigure => {
                let config = self.config.current();
                self.retire_changed_routes(&config);
                self.prune_pending_rpcs();
                Ok(())
            }
        }
    }

    pub(super) fn add_subscriber_inner(
        &mut self,
        id: u64,
        topics: Vec<ServiceTopic>,
        sink: SubscriberSink,
        expected_profile: Option<AccountProfile>,
        route_generation: NetworkRouteGeneration,
    ) -> Vec<ServiceTopic> {
        let topics = normalize_topics(&topics);
        let mut new_topics = Vec::with_capacity(topics.len());
        for topic in &topics {
            let subscribers = self.topics.entry(topic.clone()).or_default();
            if subscribers.is_empty() {
                new_topics.push(topic.clone());
            }
            subscribers.insert(id);
        }
        self.subscribers.insert(
            id,
            LocalSubscriber {
                topics: topics.clone(),
                capability: sink.capability(),
                route_generation,
                expected_profile,
                sink,
            },
        );
        if let Some(state) = self.session_state.clone() {
            self.publish_to_ids(vec![id], Ok(KytrixServiceEvent::SessionState(state)));
        }
        self.replay_latest_indices(id, &topics);
        self.replay_latest_ais(id, &topics, &new_topics);
        new_topics
    }

    pub(super) fn remote_topics(&self) -> Vec<ServiceTopic> {
        let mut topics = self.topics.keys().cloned().collect::<Vec<_>>();
        topics.sort_unstable();
        topics
    }

    pub(super) fn subscription_route_fence(&self, id: u64) -> Option<SubscriptionRouteFence> {
        self.subscribers
            .get(&id)
            .map(|subscriber| SubscriptionRouteFence {
                subscriber_id: id,
                topics: subscriber.topics.clone(),
                capability: subscriber.capability,
                route_generation: subscriber.route_generation,
            })
    }

    /// Whether the fence still names a route the latest published configuration
    /// resolves.
    ///
    /// The fence compares against the latest *published* owner projection, not
    /// against the settings a write already holds: `ConfigEventBridge` publishes
    /// during the GPUI effect flush, so a configuration write made inside the
    /// current `App::update` becomes visible to this fence at the end of that
    /// update, not synchronously.
    ///
    /// The fence must consult the current configuration, not only the subscriber
    /// map: `Command::Reconfigure` is what retires a route that a publication
    /// changed, and it cannot run while the actor is parked inside `wait_for_ack`
    /// or `await_control_with_routes`. Checking the route here is what turns that
    /// queued retire into a result the waiting operation can observe.
    pub(super) fn subscription_route_is_current(&self, fence: &SubscriptionRouteFence) -> bool {
        self.capability_route_is_current(fence.capability, fence.route_generation)
            && self
                .subscribers
                .get(&fence.subscriber_id)
                .is_some_and(|subscriber| {
                    subscriber.topics == fence.topics
                        && subscriber.capability == fence.capability
                        && subscriber.route_generation == fence.route_generation
                })
    }

    pub(super) fn profile_is_current(&self, expected: Option<&AccountProfile>) -> bool {
        expected.is_none_or(|expected| {
            self.account_profile
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(expected)
        })
    }

    pub(super) fn install_account_profile(
        &mut self,
        profile: AccountProfile,
    ) -> Result<bool, KytrixServiceError> {
        self.commit_profile_epoch(&profile)?;
        let mut current = self
            .account_profile
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.as_ref() == Some(&profile) {
            return Ok(false);
        }
        if profile.credential_revision() <= self.highest_credential_revision {
            return Err(KytrixServiceError::AccountChanged);
        }
        self.highest_credential_revision = profile.credential_revision();
        *current = Some(profile.clone());
        drop(current);
        let retired = self
            .subscribers
            .iter()
            .filter_map(|(id, subscriber)| {
                subscriber
                    .expected_profile
                    .as_ref()
                    .is_some_and(|expected| expected != &profile)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in retired {
            if let Some(subscriber) = self.subscribers.get(&id) {
                subscriber.sink.fail(TransportError::CredentialChanged);
            }
            self.queue_removed_topics(id);
        }
        Ok(true)
    }

    pub(super) fn commit_profile_epoch(
        &mut self,
        profile: &AccountProfile,
    ) -> Result<(), KytrixServiceError> {
        if !self.credential_events_open {
            return if self
                .account_profile
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(profile)
            {
                Ok(())
            } else {
                Err(KytrixServiceError::Authentication(
                    "Kytrix service credential notifications closed".into(),
                ))
            };
        }
        let channel_open = self.credential_invalidations.has_changed().is_ok();
        if !channel_open {
            self.credential_events_open = false;
        }
        let change = *self.credential_invalidations.borrow_and_update();
        if !channel_open && change == self.committed_credential_change {
            let current_is_same = self
                .account_profile
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(profile);
            return if current_is_same {
                Ok(())
            } else {
                Err(KytrixServiceError::Authentication(
                    "Kytrix service credential notifications closed".into(),
                ))
            };
        }
        if change.is_exhausted() {
            if change != self.committed_credential_change {
                self.committed_credential_change = change;
            }
            self.invalidate_credentials();
            return Err(KytrixServiceError::Authentication(
                "Kytrix service credential sequence exhausted".into(),
            ));
        }
        let Some(epoch) = change.epoch() else {
            return Err(KytrixServiceError::AccountChanged);
        };
        if epoch != profile.credential_epoch()
            || change.availability() != SessionAvailability::Available
        {
            if change != self.committed_credential_change {
                self.committed_credential_change = change;
                self.invalidate_credentials();
            }
            return Err(KytrixServiceError::AccountChanged);
        }
        if !channel_open {
            let already_installed = self
                .account_profile
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(profile);
            if already_installed {
                self.committed_credential_change = change;
                return Ok(());
            }
            self.committed_credential_change = change;
            self.invalidate_credentials();
            return Err(KytrixServiceError::Authentication(
                "Kytrix service credential notifications closed".into(),
            ));
        }
        if change != self.committed_credential_change {
            self.committed_credential_change = change;
            let already_installed = self
                .account_profile
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                == Some(profile);
            if !already_installed {
                self.clear_account_profile();
            }
        }
        Ok(())
    }

    pub(super) fn clear_account_profile(&mut self) {
        let subscriber_ids = self.subscribers.keys().copied().collect::<Vec<_>>();
        for id in subscriber_ids {
            if let Some(subscriber) = self.subscribers.get(&id) {
                subscriber.sink.fail(TransportError::CredentialChanged);
            }
            self.queue_removed_topics(id);
        }
        *self
            .account_profile
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub(super) fn retire_subscribers(&mut self, error: TransportError) {
        for subscriber in self.subscribers.values() {
            subscriber.sink.fail(error.clone());
        }
        self.subscribers.clear();
        self.topics.clear();
        self.latest_indices.clear();
        self.latest_ais_keyframes.clear();
        self.subscriptions_dirty = false;
        self.session_state = None;
        self.observed_session_state.clear(self.session_generation);
    }

    pub(super) fn invalidate_credentials(&mut self) {
        self.clear_account_profile();
        self.prepare_reconnect(KytrixServiceError::Authentication(
            "Kytrix service credentials changed".into(),
        ));
    }

    pub(super) fn commit_credential_notification(&mut self, channel_open: bool) -> bool {
        if !channel_open {
            self.credential_events_open = false;
        }
        let current = *self.credential_invalidations.borrow_and_update();
        if current == self.committed_credential_change {
            return false;
        }
        self.committed_credential_change = current;
        self.invalidate_credentials();
        true
    }

    pub(super) fn commit_pending_credential_invalidation(&mut self) -> bool {
        if !self.credential_events_open {
            return false;
        }
        match self.credential_invalidations.has_changed() {
            Ok(false) => false,
            Ok(true) => self.commit_credential_notification(true),
            Err(_) => self.commit_credential_notification(false),
        }
    }

    pub(super) fn remove_subscriber(&mut self, id: u64) -> Vec<ServiceTopic> {
        let Some(subscriber) = self.subscribers.remove(&id) else {
            return Vec::new();
        };
        let mut removed_topics = Vec::with_capacity(subscriber.topics.len());
        for topic in subscriber.topics {
            let Some(subscribers) = self.topics.get_mut(&topic) else {
                continue;
            };
            subscribers.remove(&id);
            if subscribers.is_empty() {
                self.topics.remove(&topic);
                self.clear_cached_topic(&topic);
                removed_topics.push(topic);
            }
        }
        removed_topics
    }

    pub(super) fn clear_cached_topic(&mut self, topic: &ServiceTopic) {
        match topic {
            ServiceTopic::Index(topic) => {
                self.latest_indices.remove(topic);
            }
            ServiceTopic::Ais(area) => {
                self.latest_ais_keyframes.remove(area);
            }
            ServiceTopic::News | ServiceTopic::X | ServiceTopic::Wallet => {}
        }
    }

    pub(super) fn replay_latest_indices(&mut self, id: u64, topics: &[ServiceTopic]) {
        let events = topics
            .iter()
            .filter_map(|topic| match topic {
                ServiceTopic::Index(topic) => self.latest_indices.get(topic).cloned(),
                ServiceTopic::News
                | ServiceTopic::X
                | ServiceTopic::Wallet
                | ServiceTopic::Ais(_) => None,
            })
            .collect::<Vec<_>>();
        for event in events {
            self.publish_to_ids(vec![id], Ok(KytrixServiceEvent::Index(event)));
        }
    }

    /// Hands a joining subscriber the keyframe of each AIS area the session already follows.
    ///
    /// The server replays an area's keyframe only when a connection adds that topic, and
    /// two panes of one account share this session's connection. Without this, the second
    /// pane would receive only the deltas after its arrival — none of which it can apply
    /// without the baseline — until the server's fallback keyframe, up to 300 s later. The
    /// topic had no subscribers before this one only when it is new, so a new topic is left
    /// to the server's own replay rather than being given a keyframe of unknown age.
    pub(super) fn replay_latest_ais(
        &mut self,
        id: u64,
        topics: &[ServiceTopic],
        new_topics: &[ServiceTopic],
    ) {
        for topic in topics {
            let ServiceTopic::Ais(area) = topic else {
                continue;
            };
            if new_topics.contains(topic) {
                continue;
            }
            let Some(keyframe) = self.latest_ais_keyframes.get(area).cloned() else {
                continue;
            };
            self.publish_to_ids(vec![id], Ok(KytrixServiceEvent::Ais(keyframe)));
        }
    }

    pub(super) fn prune_subscribers(&mut self) {
        let stale = self
            .subscribers
            .iter()
            .filter_map(|(id, subscriber)| subscriber.sink.is_cancelled().then_some(*id))
            .collect::<Vec<_>>();
        for id in stale {
            self.queue_removed_topics(id);
        }
    }

    pub(super) fn retire_changed_routes(&mut self, config: &Config) {
        let retired = self
            .subscribers
            .iter()
            .filter_map(|(id, subscriber)| {
                let target = ServiceTarget::KytrixService {
                    capability: subscriber.capability,
                };
                (config.service_network_route_generation(&target) != subscriber.route_generation
                    || !network::proxy::session_route_matches(
                        config,
                        &RouteTarget::Service(target),
                        &self.websocket_url,
                        &self.proxy,
                        self.os_proxy_fingerprint,
                    ))
                .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in retired {
            if let Some(subscriber) = self.subscribers.get(&id) {
                subscriber.sink.fail(TransportError::RouteChanged);
            }
            self.queue_removed_topics(id);
        }
    }

    pub(super) fn start_system_proxy_refresh(&self, task: &mut Option<JoinHandle<()>>) {
        if task.is_some() {
            return;
        }
        let config = self.config.current();
        let uses_system_route = self.subscribers.values().any(|subscriber| {
            matches!(
                config.service_proxy_route_for(&ServiceTarget::KytrixService {
                    capability: subscriber.capability,
                }),
                ProxyRoute::System
            )
        }) || self.pending_rpcs.values().any(|pending| {
            matches!(
                config.service_proxy_route_for(&ServiceTarget::KytrixService {
                    capability: pending.capability,
                }),
                ProxyRoute::System
            )
        });
        if !uses_system_route {
            return;
        }
        let cancellation = self.cancellation.clone();
        let runtime = self.runtime.clone();
        *task = Some(tokio::spawn(async move {
            let _ = network::proxy::refresh_system_proxy_fingerprint(cancellation, &runtime).await;
        }));
    }

    pub(super) fn finish_system_proxy_refresh(&mut self) {
        let config = self.config.current();
        self.retire_changed_routes(&config);
    }

    pub(super) fn capability_route_is_current(
        &self,
        capability: KytrixServiceCapability,
        route_generation: NetworkRouteGeneration,
    ) -> bool {
        let config = self.config.current();
        let target = ServiceTarget::KytrixService { capability };
        config.service_network_route_generation(&target) == route_generation
            && network::proxy::session_route_matches(
                &config,
                &RouteTarget::Service(target),
                &self.websocket_url,
                &self.proxy,
                self.os_proxy_fingerprint,
            )
    }

    pub(super) async fn await_control_with_routes<T>(
        &mut self,
        cancellation: &CancellationToken,
        deadline: Instant,
        operation: impl Future<Output = Result<T, KytrixServiceError>>,
        route_fence: Option<&SubscriptionRouteFence>,
        rpc_route_fence: Option<(KytrixServiceCapability, NetworkRouteGeneration)>,
    ) -> Result<T, KytrixServiceError> {
        if rpc_route_fence.is_some_and(|(capability, generation)| {
            !self.capability_route_is_current(capability, generation)
        }) {
            return Err(KytrixServiceError::RouteChanged);
        }
        tokio::pin!(operation);
        loop {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => return Err(KytrixServiceError::Closed),
                () = cancellation.cancelled() => return Err(KytrixServiceError::Closed),
                changed = self.credential_invalidations.changed(), if self.credential_events_open => {
                    if self.commit_credential_notification(changed.is_ok()) {
                        return Err(KytrixServiceError::Authentication(
                            "Kytrix service credentials changed".into(),
                        ));
                    }
                }
                result = timeout_at(deadline, &mut operation) => {
                    let result = result.map_err(|_| KytrixServiceError::Timeout)?;
                    if rpc_route_fence.is_some_and(|(capability, generation)| {
                        !self.capability_route_is_current(capability, generation)
                    }) || route_fence
                        .is_some_and(|fence| !self.subscription_route_is_current(fence))
                    {
                        return Err(KytrixServiceError::RouteChanged);
                    }
                    return result;
                }
            }
        }
    }

    pub(super) fn queue_removed_topics(&mut self, id: u64) {
        let topics = self.remove_subscriber(id);
        if !topics.is_empty() {
            self.subscriptions_dirty = true;
        }
    }

    pub(super) async fn flush_subscriptions(
        &mut self,
        websocket: &mut RawWebSocket,
    ) -> Result<(), KytrixServiceError> {
        if !self.subscriptions_dirty {
            return Ok(());
        }
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let topics = self.remote_topics();
        self.subscriptions_dirty = false;
        self.send_subscriptions(websocket, topics, deadline, self.cancellation.clone(), None)
            .await?;
        Ok(())
    }

    pub(super) async fn connect_websocket(
        &mut self,
        deadline: Instant,
        cancellation: CancellationToken,
        expected_profile: Option<&AccountProfile>,
    ) -> Result<(RawWebSocket, AccountProfile), KytrixServiceError> {
        let authorization_url = authorization_url(&self.websocket_url)?;
        let auth = self.auth.clone();
        let authorization = async move {
            match expected_profile {
                Some(profile) => Ok((
                    profile.clone(),
                    service_authorization_headers_for(&auth, profile, "GET", &authorization_url)
                        .await?,
                )),
                None => {
                    service_authorization_headers_with_profile(&auth, "GET", &authorization_url)
                        .await
                }
            }
        };
        self.connect_websocket_with_authorization(deadline, cancellation, authorization)
            .await
    }

    pub(super) async fn reconnect_subscribers(&mut self) -> Result<(), KytrixServiceError> {
        let profile = self
            .account_profile
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(KytrixServiceError::AccountChanged)?;
        let cancellation = self.cancellation.child_token();
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let (mut websocket, connected_profile) = self
            .connect_websocket(deadline, cancellation.clone(), Some(&profile))
            .await?;
        self.install_account_profile(connected_profile)?;
        self.send_subscriptions(
            &mut websocket,
            self.remote_topics(),
            deadline,
            cancellation,
            None,
        )
        .await?;
        for subscriber in self.subscribers.values() {
            subscriber.sink.activate();
        }
        self.websocket = Some(websocket);
        self.reset_heartbeat();
        Ok(())
    }

    pub(super) async fn connect_websocket_with_authorization(
        &mut self,
        deadline: Instant,
        cancellation: CancellationToken,
        authorization: impl Future<
            Output = Result<(AccountProfile, (String, String)), KytrixServiceError>,
        >,
    ) -> Result<(RawWebSocket, AccountProfile), KytrixServiceError> {
        let operation_cancellation = cancellation.clone();
        let (profile, (access_token, proof)) = self
            .await_control_with_routes(&operation_cancellation, deadline, authorization, None, None)
            .await?;
        self.install_account_profile(profile.clone())?;
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("DPoP {access_token}")).map_err(|_| {
                KytrixServiceError::Authentication(
                    "the DPoP access token is not a valid header value".into(),
                )
            })?,
        );
        headers.insert(
            HeaderName::from_static("dpop"),
            HeaderValue::from_str(&proof).map_err(|_| {
                KytrixServiceError::Authentication(
                    "the DPoP proof is not a valid header value".into(),
                )
            })?,
        );
        let websocket_url = self.websocket_url.clone();
        let proxy = self.proxy.clone();
        let transport_policy = self.transport_policy.clone();
        let connect_cancellation = cancellation.clone();
        let websocket = self
            .await_control_with_routes(
                &operation_cancellation,
                deadline,
                async move {
                    RawWebSocket::connect(
                        &websocket_url,
                        &proxy,
                        headers,
                        &connect_cancellation,
                        &transport_policy,
                    )
                    .await
                    .map_err(|error| {
                        KytrixServiceError::Connect(smol_str::format_smolstr!("{}", error))
                    })
                },
                None,
                None,
            )
            .await?;
        Ok((websocket, profile))
    }

    pub(super) async fn send_subscriptions(
        &mut self,
        websocket: &mut RawWebSocket,
        topics: Vec<ServiceTopic>,
        deadline: Instant,
        cancellation: CancellationToken,
        route_fence: Option<&SubscriptionRouteFence>,
    ) -> Result<(), KytrixServiceError> {
        let id = self.next_request_id();
        // Request IDs distinguish this acknowledgement from an overlapping
        // connection-level reauthentication response.
        let request = ClientRequest {
            id,
            request: Some(ClientRequestKind::SetSubscriptions(SetSubscriptions {
                topics: topics.iter().map(Into::into).collect(),
            })),
        };
        let operation_cancellation = cancellation.clone();
        self.await_control_with_routes(
            &operation_cancellation,
            deadline,
            send_frame_with_timeout(
                websocket,
                Frame::binary(Bytes::from(request.encode_to_vec())),
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(WRITE_TIMEOUT),
            ),
            route_fence,
            None,
        )
        .await?;
        self.wait_for_ack(websocket, id, &topics, deadline, cancellation, route_fence)
            .await
    }

    pub(super) async fn begin_rpc_request(
        &mut self,
        websocket: &mut RawWebSocket,
        request: BeginRpcRequest,
    ) -> Result<(), KytrixServiceError> {
        if self.pending_rpcs.len() >= MAX_PENDING_RPCS {
            let error = KytrixServiceError::Server(ErrorCode::RateLimit);
            let _ = request.respond_to.send(Err(error));
            return Ok(());
        }
        let id = self.next_request_id();
        let BeginRpcRequest {
            request,
            expected_profile,
            capability,
            route_generation,
            deadline,
            cancellation,
            possibly_submitted,
            respond_to,
        } = request;
        let request = ClientRequest { id, request };
        let send = async {
            possibly_submitted.store(true, Ordering::Release);
            send_frame_with_timeout(
                websocket,
                Frame::binary(Bytes::from(request.encode_to_vec())),
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(WRITE_TIMEOUT),
            )
            .await
        };
        if let Err(error) = self
            .await_control_with_routes(
                &cancellation,
                deadline,
                send,
                None,
                Some((capability, route_generation)),
            )
            .await
        {
            let _ = respond_to.send(Err(error.clone()));
            return Err(error);
        }
        self.pending_rpcs.insert(
            id,
            PendingRpc {
                expected_profile,
                capability,
                route_generation,
                cancellation,
                deadline,
                respond_to,
            },
        );
        Ok(())
    }

    pub(super) fn next_request_id(&mut self) -> u32 {
        loop {
            let id = if self.next_request_id == 0 {
                1
            } else {
                self.next_request_id
            };
            self.next_request_id = id.wrapping_add(1);
            if self.next_request_id == 0 {
                self.next_request_id = 1;
            }
            let renewal_uses_id = self.renewal.as_ref().is_some_and(|renewal| {
                matches!(&renewal.stage, RenewalStage::AwaitingResponse { request_id } if *request_id == id)
            });
            if !self.pending_rpcs.contains_key(&id) && !renewal_uses_id {
                return id;
            }
        }
    }

    pub(super) fn start_renewal(
        &mut self,
        challenge: proto_response::AuthChallenge,
    ) -> Result<(), KytrixServiceError> {
        if self.renewal.is_some() {
            return Err(KytrixServiceError::Protocol(
                "duplicate Kytrix service authentication challenge".into(),
            ));
        }
        if !valid_protocol_identifier(&challenge.nonce)
            || challenge.expires_at <= service_unix_now()
        {
            return Err(KytrixServiceError::Protocol(
                "invalid Kytrix service authentication challenge".into(),
            ));
        }
        let challenge = RenewalChallenge {
            nonce: challenge.nonce,
            expires_at: challenge.expires_at,
        };
        let future = self.renewal_future(&challenge)?;
        self.renewal = Some(PendingRenewal {
            deadline: service_unix_deadline(challenge.expires_at),
            challenge,
            stage: RenewalStage::Preparing(future),
        });
        Ok(())
    }

    pub(super) fn renewal_future(
        &self,
        challenge: &RenewalChallenge,
    ) -> Result<RenewalFuture, KytrixServiceError> {
        let profile = self
            .account_profile
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                KytrixServiceError::Authentication("service account profile is unavailable".into())
            })?;
        let request_url = authorization_url(&self.websocket_url)?;
        let nonce = challenge.nonce.clone();
        let auth = self.auth.clone();
        Ok(Box::pin(async move {
            service_authorization_headers_for_renewal(&auth, &profile, "GET", &request_url, &nonce)
                .await
        }))
    }

    pub(super) fn begin_renewal_retry(&mut self) -> Result<(), KytrixServiceError> {
        let challenge = self
            .renewal
            .as_ref()
            .ok_or_else(|| KytrixServiceError::Protocol("renewal state is missing".into()))?
            .challenge
            .clone();
        if service_unix_now() >= challenge.expires_at {
            return Err(KytrixServiceError::Authentication(
                "Kytrix service authentication challenge expired".into(),
            ));
        }
        let future = self.renewal_future(&challenge)?;
        let Some(renewal) = self.renewal.as_mut() else {
            return Err(KytrixServiceError::Protocol(
                "renewal state is missing".into(),
            ));
        };
        renewal.stage = RenewalStage::Preparing(future);
        Ok(())
    }

    pub(super) fn schedule_renewal_retry(&mut self) -> Result<(), KytrixServiceError> {
        let renewal = self
            .renewal
            .as_mut()
            .ok_or_else(|| KytrixServiceError::Protocol("renewal state is missing".into()))?;
        if service_unix_now() >= renewal.challenge.expires_at {
            return Err(KytrixServiceError::Authentication(
                "Kytrix service authentication challenge expired".into(),
            ));
        }
        let retry_at = (Instant::now() + RENEWAL_RETRY_INTERVAL)
            .min(service_unix_deadline(renewal.challenge.expires_at));
        renewal.stage = RenewalStage::Waiting(Box::pin(sleep_until(retry_at)));
        Ok(())
    }

    pub(super) async fn handle_renewal_poll(
        &mut self,
        websocket: &mut RawWebSocket,
        poll: RenewalPoll,
    ) -> Result<(), KytrixServiceError> {
        match poll {
            RenewalPoll::Expired => Err(KytrixServiceError::Authentication(
                "Kytrix service authentication challenge expired".into(),
            )),
            RenewalPoll::RetryReady => self.begin_renewal_retry(),
            RenewalPoll::Prepared(Err(RenewalAttemptError::Temporary)) => {
                self.schedule_renewal_retry()
            }
            RenewalPoll::Prepared(Err(RenewalAttemptError::OutcomeUnknown)) => {
                self.renewal
                    .as_mut()
                    .ok_or_else(|| KytrixServiceError::Protocol("renewal state is missing".into()))?
                    .stage = RenewalStage::WaitingForExpiry;
                Ok(())
            }
            RenewalPoll::Prepared(Err(RenewalAttemptError::Fatal(error))) => Err(error),
            RenewalPoll::Prepared(Ok((access_token, dpop_proof))) => {
                let expires_at = self
                    .renewal
                    .as_ref()
                    .ok_or_else(|| KytrixServiceError::Protocol("renewal state is missing".into()))?
                    .challenge
                    .expires_at;
                if service_unix_now() >= expires_at {
                    return Err(KytrixServiceError::Authentication(
                        "Kytrix service authentication challenge expired".into(),
                    ));
                }
                let id = self.next_request_id();
                let request = ClientRequest {
                    id,
                    request: Some(ClientRequestKind::Reauthenticate(Reauthenticate {
                        access_token,
                        dpop_proof,
                    })),
                };
                let deadline =
                    (Instant::now() + WRITE_TIMEOUT).min(service_unix_deadline(expires_at));
                let operation_cancellation = self.cancellation.clone();
                self.await_control_with_routes(
                    &operation_cancellation,
                    deadline,
                    send_frame_with_timeout(
                        websocket,
                        Frame::binary(Bytes::from(request.encode_to_vec())),
                        deadline.saturating_duration_since(Instant::now()),
                    ),
                    None,
                    None,
                )
                .await?;
                let Some(renewal) = self.renewal.as_mut() else {
                    return Err(KytrixServiceError::Protocol(
                        "renewal state is missing".into(),
                    ));
                };
                renewal.stage = RenewalStage::AwaitingResponse { request_id: id };
                renewal.deadline = service_unix_deadline(expires_at.saturating_add(
                    i64::try_from(RENEWAL_RESPONSE_GRACE.as_secs()).unwrap_or(i64::MAX),
                ));
                Ok(())
            }
        }
    }

    pub(super) fn renewal_response_id(&self) -> Option<u32> {
        match self.renewal.as_ref().map(|renewal| &renewal.stage) {
            Some(RenewalStage::AwaitingResponse { request_id }) => Some(*request_id),
            Some(
                RenewalStage::Preparing(_)
                | RenewalStage::Waiting(_)
                | RenewalStage::WaitingForExpiry,
            )
            | None => None,
        }
    }

    pub(super) fn accept_renewal_response(
        &mut self,
        response: proto_response::Response,
    ) -> Result<(), KytrixServiceError> {
        if self
            .renewal
            .as_ref()
            .is_some_and(|renewal| Instant::now() >= renewal.deadline)
        {
            return Err(KytrixServiceError::Authentication(
                "Kytrix service authentication renewal expired".into(),
            ));
        }
        let Some(expected_id) = self.renewal_response_id() else {
            return Err(KytrixServiceError::Protocol(
                "unexpected Kytrix service response".into(),
            ));
        };
        if response.id != expected_id {
            return Err(KytrixServiceError::Protocol(
                "Kytrix service reauthentication response id mismatch".into(),
            ));
        }
        match response.response {
            Some(proto_response::response::Response::Reauthenticate(_)) => {
                self.renewal = None;
                Ok(())
            }
            Some(proto_response::response::Response::Error(error)) => {
                let code = ErrorCode::try_from(error.code).map_err(|_| {
                    KytrixServiceError::Protocol("unknown service error code".into())
                })?;
                if code == ErrorCode::AuthenticationUnavailable {
                    self.schedule_renewal_retry()
                } else {
                    Err(KytrixServiceError::Server(code))
                }
            }
            Some(_) | None => Err(KytrixServiceError::Protocol(
                "invalid Kytrix service reauthentication response".into(),
            )),
        }
    }

    pub(super) async fn wait_for_ack(
        &mut self,
        websocket: &mut RawWebSocket,
        expected_id: u32,
        expected_topics: &[ServiceTopic],
        deadline: Instant,
        cancellation: CancellationToken,
        route_fence: Option<&SubscriptionRouteFence>,
    ) -> Result<(), KytrixServiceError> {
        loop {
            let can_poll_renewal = renewal_pollable(&self.renewal);
            let incoming = tokio::select! {
                biased;
                () = self.cancellation.cancelled() => return Err(KytrixServiceError::Closed),
                () = cancellation.cancelled() => return Err(KytrixServiceError::Closed),
                changed = self.credential_invalidations.changed(), if self.credential_events_open => {
                    if self.commit_credential_notification(changed.is_ok()) {
                        return Err(KytrixServiceError::Authentication(
                            "Kytrix service credentials changed".into(),
                        ));
                    }
                    continue;
                }
                () = self.wake.notified() => {
                    self.prune_subscribers();
                    continue;
                }
                renewal = poll_renewal(&mut self.renewal), if can_poll_renewal => {
                    self.handle_renewal_poll(websocket, renewal).await?;
                    continue;
                }
                result = timeout_at(deadline, next_server_message(websocket)) => {
                    result.map_err(|_| KytrixServiceError::Timeout)??
                }
            };
            match incoming {
                IncomingMessage::Response(response) => {
                    if response.id == expected_id {
                        validate_response(response, expected_id, expected_topics)?;
                        if route_fence
                            .is_some_and(|fence| !self.subscription_route_is_current(fence))
                        {
                            self.subscription_route_change_reuses_socket = true;
                            return Err(KytrixServiceError::RouteChanged);
                        }
                        return Ok(());
                    }
                    if self.complete_pending_rpc(response.clone()) {
                        continue;
                    }
                    self.accept_renewal_response(response)?;
                }
                IncomingMessage::Event(Some((topic, event))) => {
                    self.publish_topic(topic, Ok(event));
                }
                IncomingMessage::Event(None) => {}
                IncomingMessage::SessionState(state) => self.accept_session_state(state),
                IncomingMessage::AuthChallenge(challenge) => self.start_renewal(challenge)?,
                IncomingMessage::Pong(payload) => self.accept_pong(&payload),
            }
        }
    }

    pub(super) async fn process_frame(
        &mut self,
        websocket: &mut RawWebSocket,
        frame: Frame,
    ) -> Result<FrameDisposition, KytrixServiceError> {
        match frame.opcode() {
            OpCode::Ping => {
                let operation_cancellation = self.cancellation.clone();
                self.await_control_with_routes(
                    &operation_cancellation,
                    Instant::now() + WRITE_TIMEOUT,
                    flush_protocol_frames(websocket, WRITE_TIMEOUT),
                    None,
                    None,
                )
                .await
                .map(|()| FrameDisposition::Continue)
            }
            OpCode::Pong => {
                self.accept_pong(frame.payload());
                Ok(FrameDisposition::Continue)
            }
            OpCode::Close => Err(KytrixServiceError::Disconnected),
            OpCode::Text | OpCode::Continuation => Err(KytrixServiceError::Protocol(
                "binary protobuf frames required".into(),
            )),
            OpCode::Binary => self.process_binary_payload(frame.payload()),
        }
    }

    pub(super) fn process_binary_payload(
        &mut self,
        payload: &[u8],
    ) -> Result<FrameDisposition, KytrixServiceError> {
        if self.commit_pending_credential_invalidation() {
            self.renewal = None;
            return Ok(FrameDisposition::CredentialsChanged);
        }
        self.publish_frame_after_fence(decode_binary(payload)?)?;
        Ok(FrameDisposition::Continue)
    }

    pub(super) fn publish_frame_after_fence(
        &mut self,
        message: IncomingMessage,
    ) -> Result<(), KytrixServiceError> {
        match message {
            IncomingMessage::Response(response) => {
                if self.complete_pending_rpc(response.clone()) {
                    return Ok(());
                }
                self.accept_renewal_response(response)
            }
            IncomingMessage::Event(Some((topic, event))) => {
                self.publish_topic(topic, Ok(event));
                Ok(())
            }
            IncomingMessage::SessionState(state) => {
                self.accept_session_state(state);
                Ok(())
            }
            IncomingMessage::AuthChallenge(challenge) => self.start_renewal(challenge),
            IncomingMessage::Event(None) | IncomingMessage::Pong(_) => Ok(()),
        }
    }

    pub(super) fn complete_pending_rpc(&mut self, response: proto_response::Response) -> bool {
        let Some(pending) = self.pending_rpcs.remove(&response.id) else {
            return false;
        };
        let result = if pending.cancellation.is_cancelled() {
            Err(KytrixServiceError::Closed)
        } else if Instant::now() >= pending.deadline {
            Err(KytrixServiceError::Timeout)
        } else if !self.profile_is_current(Some(&pending.expected_profile)) {
            Err(KytrixServiceError::AccountChanged)
        } else if !self.capability_route_is_current(pending.capability, pending.route_generation) {
            Err(KytrixServiceError::RouteChanged)
        } else {
            validate_rpc_response(response)
        };
        let _ = pending.respond_to.send(result);
        true
    }

    pub(super) fn prune_pending_rpcs(&mut self) {
        let now = Instant::now();
        let ids = self
            .pending_rpcs
            .iter()
            .filter_map(|(id, pending)| {
                (pending.cancellation.is_cancelled()
                    || now >= pending.deadline
                    || !self.profile_is_current(Some(&pending.expected_profile))
                    || !self
                        .capability_route_is_current(pending.capability, pending.route_generation))
                .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in ids {
            let Some(pending) = self.pending_rpcs.remove(&id) else {
                continue;
            };
            let error = if pending.cancellation.is_cancelled() {
                KytrixServiceError::Closed
            } else if now >= pending.deadline {
                KytrixServiceError::Timeout
            } else if !self.profile_is_current(Some(&pending.expected_profile)) {
                KytrixServiceError::AccountChanged
            } else {
                KytrixServiceError::RouteChanged
            };
            let _ = pending.respond_to.send(Err(error));
        }
    }

    pub(super) fn fail_pending_rpcs(&mut self, error: KytrixServiceError) {
        for (_, pending) in self.pending_rpcs.drain() {
            let _ = pending.respond_to.send(Err(error.clone()));
        }
    }

    pub(super) fn accept_session_state(&mut self, state: KytrixSessionState) {
        self.session_state = Some(state.clone());
        self.observed_session_state
            .publish(self.session_generation, state.clone());
        self.publish_all(Ok(KytrixServiceEvent::SessionState(state)));
    }

    pub(super) fn prepare_reconnect(&mut self, error: KytrixServiceError) {
        self.fail_pending_rpcs(match &error {
            KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
                KytrixServiceError::AccountChanged
            }
            KytrixServiceError::RouteChanged => KytrixServiceError::RouteChanged,
            _ => KytrixServiceError::Disconnected,
        });
        self.subscription_route_change_reuses_socket = false;
        if matches!(
            error,
            KytrixServiceError::Authentication(_) | KytrixServiceError::AccountChanged
        ) {
            self.retire_subscribers(TransportError::CredentialChanged);
            self.pending_pong = None;
            self.renewal = None;
            return;
        }
        for subscriber in self.subscribers.values() {
            subscriber.sink.reconnecting();
        }
        self.latest_indices.clear();
        self.latest_ais_keyframes.clear();
        self.session_state = None;
        self.observed_session_state.clear(self.session_generation);
        self.subscriptions_dirty = false;
        self.pending_pong = None;
        self.renewal = None;
        self.sweep_deadline = Instant::now() + SUBSCRIBER_SWEEP_INTERVAL;
    }

    pub(super) fn reset_heartbeat(&mut self) {
        self.pending_pong = None;
        self.heartbeat_deadline = Instant::now() + heartbeat_delay();
    }

    pub(super) fn accept_pong(&mut self, payload: &[u8]) {
        if self
            .pending_pong
            .as_ref()
            .is_some_and(|token| payload == token)
        {
            self.reset_heartbeat();
        }
    }

    pub(super) fn publish_topic(
        &mut self,
        topic: ServiceTopic,
        message: Result<KytrixServiceEvent, KytrixServiceError>,
    ) {
        if let ServiceTopic::Ais(area) = &topic
            && let Ok(KytrixServiceEvent::Ais(update)) = &message
            && update.keyframe
        {
            self.latest_ais_keyframes.insert(*area, update.clone());
        }
        if self.topics.contains_key(&topic)
            && let (ServiceTopic::Index(index_topic), Ok(KytrixServiceEvent::Index(event))) =
                (&topic, &message)
            && &event.topic == index_topic
        {
            self.latest_indices
                .insert(index_topic.clone(), event.clone());
        }
        let ids = self
            .topics
            .get(&topic)
            .map(|ids| ids.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        self.publish_to_ids(ids, message);
    }

    pub(super) fn publish_all(&mut self, message: Result<KytrixServiceEvent, KytrixServiceError>) {
        let ids = self.subscribers.keys().copied().collect::<Vec<_>>();
        self.publish_to_ids(ids, message);
    }

    pub(super) fn publish_to_ids(
        &mut self,
        ids: Vec<u64>,
        message: Result<KytrixServiceEvent, KytrixServiceError>,
    ) {
        let mut stale = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(subscriber) = self.subscribers.get(&id) else {
                continue;
            };
            if subscriber.sink.is_cancelled() {
                stale.push((id, false));
                continue;
            }
            if subscriber
                .expected_profile
                .as_ref()
                .is_some_and(|profile| !self.profile_is_current(Some(profile)))
            {
                subscriber.sink.fail(TransportError::CredentialChanged);
                stale.push((id, false));
                continue;
            }
            let send_failed = match &message {
                Ok(event) => subscriber.sink.try_send(event).is_err(),
                Err(error) => {
                    subscriber.sink.fail(subscription_transport_error(error));
                    true
                }
            };
            if send_failed {
                stale.push((id, true));
            }
        }
        for (id, _) in stale {
            self.queue_removed_topics(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{super::test_support::*, *};

    #[test]
    fn actor_should_reference_count_duplicate_local_topics() {
        let (commands, receiver) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let mut actor = ServiceSessionActor::new(receiver, cancellation, Arc::new(Notify::new()));
        let (first_publisher, _first) =
            network::transport::subscription::subscription_channel(1).unwrap();
        let (second_publisher, _second) =
            network::transport::subscription::subscription_channel(1).unwrap();
        let topic = ServiceTopic::News;

        assert_eq!(
            actor.add_subscriber_inner(
                1,
                vec![topic.clone()],
                SubscriberSink::News(first_publisher),
                None,
                NetworkRouteGeneration::default(),
            ),
            vec![topic.clone()]
        );
        assert!(
            actor
                .add_subscriber_inner(
                    2,
                    vec![topic.clone()],
                    SubscriberSink::News(second_publisher),
                    None,
                    NetworkRouteGeneration::default(),
                )
                .is_empty()
        );
        assert!(actor.remove_subscriber(1).is_empty());
        assert_eq!(actor.remove_subscriber(2), vec![topic]);
        drop(commands);
    }

    #[test]
    fn actor_should_replay_only_cached_index_events_and_clear_the_last_reference() {
        let (_commands, receiver) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let mut actor = ServiceSessionActor::new(receiver, cancellation, Arc::new(Notify::new()));
        let index = index_topic("HSI");
        let topic = ServiceTopic::Index(index.clone());
        let event = IndexEvent {
            topic: index.clone(),
            last_price: 100,
            open_price: 90,
            high_price: 110,
            low_price: 80,
            close_price: 95,
            is_trading: true,
            timestamp: 1,
        };
        let (first_publisher, mut first_events) =
            network::transport::subscription::subscription_channel(4).unwrap();
        actor.add_subscriber_inner(
            1,
            vec![topic.clone()],
            SubscriberSink::Indices(first_publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        actor.publish_topic(topic.clone(), Ok(KytrixServiceEvent::Index(event.clone())));
        assert_eq!(first_events.try_recv_for_test(), Ok(event.clone()));

        let (second_publisher, mut second_events) =
            network::transport::subscription::subscription_channel(4).unwrap();
        actor.add_subscriber_inner(
            2,
            vec![topic.clone()],
            SubscriberSink::Indices(second_publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        assert_eq!(second_events.try_recv_for_test(), Ok(event));
        assert!(actor.remove_subscriber(1).is_empty());
        assert!(actor.latest_indices.contains_key(&index));
        assert_eq!(actor.remove_subscriber(2), vec![topic]);
        assert!(!actor.latest_indices.contains_key(&index));

        let news_topic = ServiceTopic::News;
        let news = KytrixServiceEvent::News(NewsEvent {
            timestamp: 1,
            title: None,
            content: "news".into(),
            category: NewsKind::General,
        });
        let (news_publisher, mut news_events) =
            network::transport::subscription::subscription_channel(2).unwrap();
        actor.add_subscriber_inner(
            3,
            vec![news_topic.clone()],
            SubscriberSink::News(news_publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        actor.publish_topic(news_topic.clone(), Ok(news));
        assert!(news_events.try_recv_for_test().is_ok());
        let (late_news_publisher, mut late_news_events) =
            network::transport::subscription::subscription_channel(2).unwrap();
        actor.add_subscriber_inner(
            4,
            vec![news_topic],
            SubscriberSink::News(late_news_publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        assert!(matches!(
            late_news_events.try_recv_for_test(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn a_second_ais_subscriber_should_be_handed_the_keyframe_it_missed() {
        use crate::service::ais::{AisArea, AisPosition, AisUpdate};

        let (_commands, receiver) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let mut actor = ServiceSessionActor::new(receiver, cancellation, Arc::new(Notify::new()));
        let topic = ServiceTopic::Ais(AisArea::Hormuz);

        let keyframe = AisUpdate {
            epoch: 7,
            keyframe: true,
            sequence: 3,
            base: 0,
            statics: Vec::new(),
            positions: vec![AisPosition {
                mmsi: 42,
                latitude: 26_500_000,
                longitude: 56_300_000,
                sog: 100,
                cog: 900,
                heading: 90,
                nav_status: 0,
                last_update: 1_700_000_000_000,
            }],
            removed: Vec::new(),
            timestamp: 1_700_000_000_000,
        };

        let (first_publisher, mut first_events) =
            network::transport::subscription::subscription_channel(4).unwrap();
        actor.add_subscriber_inner(
            1,
            vec![topic.clone()],
            SubscriberSink::Ais(first_publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        actor.publish_topic(topic.clone(), Ok(KytrixServiceEvent::Ais(keyframe.clone())));
        assert_eq!(first_events.try_recv_for_test(), Ok(keyframe.clone()));

        // The topic was already subscribed, so the server is not told the second pane
        // joined and will not replay its keyframe: this session has to.
        let (second_publisher, mut second_events) =
            network::transport::subscription::subscription_channel(4).unwrap();
        actor.add_subscriber_inner(
            2,
            vec![topic.clone()],
            SubscriberSink::Ais(second_publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        assert_eq!(
            second_events.try_recv_for_test(),
            Ok(keyframe),
            "the joining pane should start from the picture already held",
        );

        assert!(actor.remove_subscriber(1).is_empty());
        assert!(
            actor.latest_ais_keyframes.contains_key(&AisArea::Hormuz),
            "the pane still reading the area keeps its keyframe",
        );
        assert_eq!(actor.remove_subscriber(2), vec![topic]);
        assert!(
            !actor.latest_ais_keyframes.contains_key(&AisArea::Hormuz),
            "the last subscriber taking the area away takes its keyframe too",
        );
    }

    #[test]
    fn network_failure_should_keep_subscribers_for_same_route_replay() {
        let (_commands, receiver) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let mut actor = ServiceSessionActor::new(receiver, cancellation, Arc::new(Notify::new()));
        let (publisher, mut events) =
            network::transport::subscription::subscription_channel(2).unwrap();
        let old_index = index_topic("DJI");
        let old_topic = ServiceTopic::Index(old_index.clone());
        actor.add_subscriber_inner(
            1,
            vec![old_topic.clone()],
            SubscriberSink::Indices(publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        actor.publish_topic(
            old_topic.clone(),
            Ok(KytrixServiceEvent::Index(IndexEvent {
                topic: old_index.clone(),
                last_price: 100,
                open_price: 90,
                high_price: 110,
                low_price: 80,
                close_price: 95,
                is_trading: true,
                timestamp: 1,
            })),
        );
        assert!(events.try_recv_for_test().is_ok());
        assert!(actor.latest_indices.contains_key(&old_index));
        actor.subscriptions_dirty = true;

        actor.prepare_reconnect(KytrixServiceError::Read("network reset".into()));

        assert_eq!(
            events.status(),
            network::transport::subscription::SubscriptionStatus::Reconnecting
        );
        assert_eq!(actor.subscribers.len(), 1);
        assert_eq!(actor.topics.len(), 1);
        assert!(actor.topics.contains_key(&old_topic));
        assert!(actor.latest_indices.is_empty());
        assert!(!actor.subscriptions_dirty);
    }

    #[test]
    fn subscription_route_fence_should_follow_the_current_configuration() {
        let (_commands, receiver) = mpsc::channel(1);
        let mut manager = TestConfigManager::in_memory(Config::default());
        let mut actor =
            ServiceSessionActor::new(receiver, CancellationToken::new(), Arc::new(Notify::new()));
        actor.config = manager.notification();
        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };
        let (publisher, _subscription) =
            network::transport::subscription::subscription_channel(2).unwrap();
        actor.add_subscriber_inner(
            1,
            vec![ServiceTopic::News],
            SubscriberSink::News(publisher),
            None,
            manager.read().service_network_route_generation(&target),
        );
        let fence = actor
            .subscription_route_fence(1)
            .expect("the subscriber should have a route fence");
        assert!(actor.subscription_route_is_current(&fence));

        manager.update(|config| {
            config.proxies.insert(
                "alternate".into(),
                app_config::network::ProxyConfig {
                    addr: "127.0.0.1".into(),
                    port: 9,
                    authorization: app_config::network::ProxyAuthorizationConfig::None,
                    resolve_dns_at_remote: false,
                },
            );
            config
                .proxy_applications
                .insert(target, ProxyRoute::named("alternate"));
        });

        // The subscriber entry is untouched; only the configuration moved. A
        // fence that read the subscriber map alone would still accept it.
        assert!(actor.subscribers.contains_key(&1));
        assert!(
            !actor.subscription_route_is_current(&fence),
            "a route that moved while a reconfigure was queued must fail the fence"
        );
    }

    #[test]
    fn topic_normalization_should_sort_and_deduplicate_topics() {
        let wti = index_service_topic("WTI");
        let topics = normalize_topics(&[wti.clone(), ServiceTopic::News, wti.clone()]);

        assert_eq!(topics, vec![ServiceTopic::News, wti]);
    }

    #[test]
    fn cancelled_subscriber_should_queue_remote_topic_cleanup() {
        let (_commands, receiver) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let mut actor = ServiceSessionActor::new(receiver, cancellation, Arc::new(Notify::new()));
        let (publisher, subscription) =
            network::transport::subscription::subscription_channel(1).unwrap();
        let topic = ServiceTopic::News;

        actor.add_subscriber_inner(
            1,
            vec![topic],
            SubscriberSink::News(publisher),
            None,
            NetworkRouteGeneration::default(),
        );
        drop(subscription);
        actor.prune_subscribers();

        assert!(actor.subscribers.is_empty());
        assert!(actor.topics.is_empty());
        assert!(actor.subscriptions_dirty);
        assert!(actor.remote_topics().is_empty());
    }

    #[test]
    fn cancel_on_drop_should_cancel_subscribe_token() {
        let token = CancellationToken::new();
        {
            let _cancel_on_drop = CancelOnDrop(token.clone());
        }
        assert!(token.is_cancelled());
    }

    #[test]
    fn disarmed_cancel_on_drop_should_leave_token_live() {
        let token = CancellationToken::new();
        {
            let mut cancel_on_drop = CancelOnDrop(token.clone());
            cancel_on_drop.disarm();
        }
        assert!(!token.is_cancelled());
    }

    #[test]
    fn request_ids_should_skip_zero_and_reserved_ids_after_wraparound() {
        let (_commands, receiver) = mpsc::channel(1);
        let mut actor =
            ServiceSessionActor::new(receiver, CancellationToken::new(), Arc::new(Notify::new()));
        actor.next_request_id = u32::MAX;
        let (respond_to, _response) = oneshot::channel();
        actor.pending_rpcs.insert(
            u32::MAX,
            PendingRpc {
                expected_profile: AccountProfile::new(TEST_ACCOUNT_ID, 1, 1),
                capability: KytrixServiceCapability::CryptocurrencyEtf,
                route_generation: NetworkRouteGeneration::default(),
                cancellation: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(1),
                respond_to,
            },
        );
        actor.renewal = Some(PendingRenewal {
            challenge: RenewalChallenge {
                nonce: "request-id-test".to_owned(),
                expires_at: service_unix_now() + 30,
            },
            stage: RenewalStage::AwaitingResponse { request_id: 1 },
            deadline: Instant::now() + Duration::from_secs(1),
        });

        assert_eq!(actor.next_request_id(), 2);
        actor.pending_rpcs.clear();
        actor.renewal = None;
        actor.next_request_id = u32::MAX;
        assert_eq!(actor.next_request_id(), u32::MAX);
        assert_eq!(actor.next_request_id(), 1);
    }

    #[test]
    fn duplicate_authentication_challenge_is_a_protocol_error() {
        let (_commands, receiver) = mpsc::channel(1);
        let (_invalidations, invalidation_receiver) = credential_change_watch();
        let mut actor = ServiceSessionActor::with_url(
            receiver,
            CancellationToken::new(),
            Arc::new(Notify::new()),
            KYTRIX_SERVICE_WS_URL.clone(),
            Arc::new(RwLock::new(None)),
            invalidation_receiver,
        );
        actor
            .install_account_profile(AccountProfile::new(TEST_ACCOUNT_ID, 1, 1))
            .unwrap();
        let challenge = proto_response::AuthChallenge {
            nonce: "challenge-nonce".to_owned(),
            expires_at: service_unix_now() + 30,
        };
        actor.start_renewal(challenge.clone()).unwrap();

        assert!(matches!(
            actor.start_renewal(challenge),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn reauthentication_response_id_mismatch_is_a_protocol_error() {
        let (_commands, receiver) = mpsc::channel(1);
        let (_invalidations, invalidation_receiver) = credential_change_watch();
        let mut actor = ServiceSessionActor::with_url(
            receiver,
            CancellationToken::new(),
            Arc::new(Notify::new()),
            KYTRIX_SERVICE_WS_URL.clone(),
            Arc::new(RwLock::new(None)),
            invalidation_receiver,
        );
        actor
            .install_account_profile(AccountProfile::new(TEST_ACCOUNT_ID, 1, 1))
            .unwrap();
        actor
            .start_renewal(proto_response::AuthChallenge {
                nonce: "challenge-nonce".to_owned(),
                expires_at: service_unix_now() + 30,
            })
            .unwrap();
        actor.renewal.as_mut().unwrap().stage = RenewalStage::AwaitingResponse { request_id: 7 };

        assert!(matches!(
            actor.accept_renewal_response(proto_response::Response {
                id: 8,
                response: Some(proto_response::response::Response::Reauthenticate(
                    proto_response::ReauthenticateResponse {},
                )),
            }),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn heartbeat_should_accept_only_the_matching_pong_payload() {
        let (_commands, receiver) = mpsc::channel(1);
        let mut actor =
            ServiceSessionActor::new(receiver, CancellationToken::new(), Arc::new(Notify::new()));
        let token = 42_u64.to_be_bytes();
        actor.pending_pong = Some(token);

        actor.accept_pong(&41_u64.to_be_bytes());
        assert_eq!(actor.pending_pong, Some(token));

        actor.accept_pong(&token);
        assert_eq!(actor.pending_pong, None);
    }

    #[test]
    fn heartbeat_delay_should_stay_within_server_window() {
        for _ in 0..100 {
            assert!(
                (HEARTBEAT_MIN_SECONDS..=HEARTBEAT_MAX_SECONDS)
                    .contains(&heartbeat_delay().as_secs())
            );
        }
    }
}

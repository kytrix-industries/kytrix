use std::{collections::hash_map::Entry, sync::RwLock};

use ahash::AHashMap;
use app_config::network::{Config, RouteTarget, ServiceTarget};
use network::transport::websocket::WsSessionKey;
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, timeout},
};
use url::Url;

use crate::{
    account_profile::AccountProfile,
    auth::model::{SessionAvailability, SessionChange},
    service::engine::{
        COMMAND_CAPACITY, Command, KytrixServiceError, OPERATION_TIMEOUT, ServiceSessionActor,
        ServiceSessionContext, ServiceSessionEntry, ServiceSessionExit, SupervisorCommand,
        kytrix_service_endpoint, subscription_transport_error,
    },
};

pub(super) async fn run_service_supervisor(
    mut commands: mpsc::Receiver<SupervisorCommand>,
    context: ServiceSessionContext,
) {
    let shared_context = context.clone();
    let ServiceSessionContext {
        websocket_url,
        config,
        account_profile,
        mut credential_invalidations,
        session_state,
        cancellation,
        wake,
        ..
    } = context;
    let mut sessions = AHashMap::<WsSessionKey, ServiceSessionEntry>::with_capacity(8);
    let mut tasks = JoinSet::<ServiceSessionExit>::new();
    let mut next_generation = 1_u64;
    let mut highest_credential_revision = 0_u64;
    let mut committed_credential_change = *credential_invalidations.borrow_and_update();
    let mut credential_events_open = true;

    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(exit)) = result
                    && sessions.get(&exit.key).is_some_and(|entry| entry.generation == exit.generation)
                {
                    sessions.remove(&exit.key);
                }
            }
            changed = credential_invalidations.changed(), if credential_events_open => {
                if changed.is_err() {
                    credential_events_open = false;
                }
                let current = *credential_invalidations.borrow_and_update();
                if current != committed_credential_change || !credential_events_open {
                    committed_credential_change = current;
                    *account_profile
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    retire_service_sessions(&sessions).await;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    SupervisorCommand::Send { key, profile, command } => {
                        let route = validate_service_subscription_route(
                            &key,
                            &command,
                            &websocket_url,
                            &config.current(),
                        );
                        if let Err(error) = route {
                            reject_session_command(*command, error);
                            continue;
                        }
                        match install_supervisor_profile(
                            &account_profile,
                            &mut highest_credential_revision,
                            committed_credential_change,
                            profile,
                        ) {
                            Ok(replaced) => {
                                if replaced {
                                    retire_service_sessions(&sessions).await;
                                }
                                dispatch_service_subscription(
                                    key,
                                    *command,
                                    &mut sessions,
                                    &mut tasks,
                                    &mut next_generation,
                                    &shared_context,
                                )
                                .await;
                            }
                            Err(error) => reject_session_command(*command, error),
                        }
                    }
                    SupervisorCommand::AcquireAccount {
                        profile,
                        cancellation: operation_cancellation,
                        deadline,
                        respond_to,
                    } => {
                        let result = if operation_cancellation.is_cancelled() {
                            Err(KytrixServiceError::Closed)
                        } else if Instant::now() >= deadline {
                            Err(KytrixServiceError::Timeout)
                        } else {
                            match install_supervisor_profile(
                                &account_profile,
                                &mut highest_credential_revision,
                                committed_credential_change,
                                profile,
                            ) {
                                Ok(replaced) => {
                                    if replaced {
                                        retire_service_sessions(&sessions).await;
                                    }
                                    Ok(())
                                }
                                Err(error) => Err(error),
                            }
                        };
                        let _ = respond_to.send(result);
                    }
                    SupervisorCommand::Reconfigure => {
                        for (key, entry) in &sessions {
                            if timeout(
                                OPERATION_TIMEOUT,
                                entry.commands.send(Command::Reconfigure),
                            )
                            .await
                            .is_err()
                            {
                                log::warn!(
                                    "failed to deliver a Kytrix service configuration change to the session {key:?} within {OPERATION_TIMEOUT:?}; its subscriptions keep the previous routing until the next command"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    commands.close();
    cancellation.cancel();
    wake.notify_waiters();
    while tasks.join_next().await.is_some() {}
    while let Ok(command) = commands.try_recv() {
        reject_supervisor_command(command, KytrixServiceError::Closed);
    }
    session_state.clear_all();
}

fn install_supervisor_profile(
    account_profile: &RwLock<Option<AccountProfile>>,
    highest_credential_revision: &mut u64,
    credential_change: SessionChange,
    profile: AccountProfile,
) -> Result<bool, KytrixServiceError> {
    if credential_change.availability() != SessionAvailability::Available
        || credential_change.epoch() != Some(profile.credential_epoch())
    {
        return Err(KytrixServiceError::AccountChanged);
    }
    let mut current = account_profile
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current.as_ref() == Some(&profile) {
        return Ok(false);
    }
    if *highest_credential_revision != 0
        && profile.credential_revision() <= *highest_credential_revision
    {
        return Err(KytrixServiceError::AccountChanged);
    }
    let replaced = current.is_some();
    *highest_credential_revision =
        (*highest_credential_revision).max(profile.credential_revision());
    *current = Some(profile);
    Ok(replaced)
}

async fn retire_service_sessions(sessions: &AHashMap<WsSessionKey, ServiceSessionEntry>) {
    for entry in sessions.values() {
        let _ = timeout(
            OPERATION_TIMEOUT,
            entry.commands.send(Command::RetireCredentials),
        )
        .await;
    }
}

async fn dispatch_service_subscription(
    key: WsSessionKey,
    command: Command,
    sessions: &mut AHashMap<WsSessionKey, ServiceSessionEntry>,
    tasks: &mut JoinSet<ServiceSessionExit>,
    next_generation: &mut u64,
    context: &ServiceSessionContext,
) {
    loop {
        match sessions.entry(key.clone()) {
            Entry::Occupied(entry) => {
                let sender = entry.get().commands.clone();
                match timeout(OPERATION_TIMEOUT, sender.reserve_owned()).await {
                    Ok(Ok(permit)) => {
                        permit.send(command);
                        return;
                    }
                    Ok(Err(_)) => {
                        entry.remove();
                    }
                    Err(_) => {
                        reject_session_command(command, KytrixServiceError::Timeout);
                        return;
                    }
                }
            }
            Entry::Vacant(entry) => {
                let generation = *next_generation;
                *next_generation = next_generation.wrapping_add(1).max(1);
                let (session_commands, session_command_receiver) = mpsc::channel(COMMAND_CAPACITY);
                let mut actor_context = context.clone();
                actor_context.cancellation = context.cancellation.child_token();
                let actor = ServiceSessionActor::from_context(
                    session_command_receiver,
                    actor_context,
                    key.proxy.clone(),
                    key.os_proxy_fingerprint,
                    generation,
                );
                let exit_key = key.clone();
                tasks.spawn(async move {
                    actor.run().await;
                    ServiceSessionExit {
                        key: exit_key,
                        generation,
                    }
                });
                entry.insert(ServiceSessionEntry {
                    generation,
                    commands: session_commands,
                });
            }
        }
    }
}

fn reject_supervisor_command(command: SupervisorCommand, error: KytrixServiceError) {
    match command {
        SupervisorCommand::Send { command, .. } => reject_session_command(*command, error),
        SupervisorCommand::AcquireAccount { respond_to, .. } => {
            let _ = respond_to.send(Err(error));
        }
        SupervisorCommand::Reconfigure => {}
    }
}

fn reject_session_command(command: Command, error: KytrixServiceError) {
    match command {
        Command::Subscribe {
            sink, respond_to, ..
        } => {
            sink.fail(subscription_transport_error(&error));
            let _ = respond_to.send(Err(error));
        }
        Command::Rpc { respond_to, .. } => {
            let _ = respond_to.send(Err(error));
        }
        Command::RetireCredentials => {}
        Command::Reconfigure => {}
    }
}

fn validate_service_subscription_route(
    key: &WsSessionKey,
    command: &Command,
    websocket_url: &Url,
    config: &Config,
) -> Result<(), KytrixServiceError> {
    let (capability, route_generation) = match command {
        Command::Subscribe {
            sink,
            route_generation,
            ..
        } => (sink.capability(), route_generation),
        Command::Rpc {
            capability,
            route_generation,
            ..
        } => (*capability, route_generation),
        _ => return Ok(()),
    };
    let expected_endpoint = kytrix_service_endpoint(websocket_url)?;
    if key.endpoint != expected_endpoint {
        return Err(KytrixServiceError::AccountChanged);
    }
    let target = ServiceTarget::KytrixService { capability };
    if config.service_network_route_generation(&target) != *route_generation {
        return Err(KytrixServiceError::RouteChanged);
    }
    if !network::proxy::session_route_matches(
        config,
        &RouteTarget::Service(target),
        websocket_url,
        &key.proxy,
        key.os_proxy_fingerprint,
    ) {
        return Err(KytrixServiceError::RouteChanged);
    }
    Ok(())
}

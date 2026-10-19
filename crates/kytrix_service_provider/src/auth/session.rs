use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use app_config::network::Config;
use app_config_provider::notification::ConfigNotification;
use network::transport::hub::TransportHub;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use super::{
    ACCOUNT_ORIGIN, AUTH_COMMAND_CAPACITY, AccountIdentity, AuthAuthority, AuthCommand, AuthError,
    AuthorizationProofContext, CREDENTIAL_SERVICE, CREDENTIAL_USER, CredentialStore, DpopKey,
    KeyringCredentialStore, LoadedCredential, LocalSessionStatus, OAuthAuthority, PlanCode,
    REFRESH_EARLY_SECS, RENEWAL_REFRESH_EARLY_SECS, SessionChange, SessionControl, SessionState,
    TokenResponse, authorization_headers_inner, current_plan, current_profile,
    ensure_session_loaded, install_session, load_credential, local_session_status_inner,
    retire_stored_credential, validate_account,
};
use crate::account_profile::AccountProfile;

/// The credential a sign-out pulled out of the session, kept only long enough to tell
/// the authority to drop it.
///
/// It is deliberately not the session: the sign-out is already finished as far as this
/// process is concerned, and nothing here can bring it back.
#[derive(Clone)]
pub struct Revocation {
    refresh_token: String,
    key: DpopKey,
}

impl Revocation {
    /// Builds the token a sign-out would hand over, for a caller that has no session to
    /// take one from.
    pub fn new(refresh_token: impl Into<String>, key: DpopKey) -> Self {
        Self {
            refresh_token: refresh_token.into(),
            key,
        }
    }

    /// The token the authority is to be told about.
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }
}

/// What a sign-out did on this machine.
pub struct DiscardedSession {
    /// The token the authority still holds, when the session had one to hand over.
    revocation: Option<Revocation>,
    /// Whether the credential store is known to hold nothing that grants access.
    removed: bool,
}

impl DiscardedSession {
    /// Whether the credential store is known to hold nothing of this account any more.
    ///
    /// A store that refused both the replacement and the removal is the one case where
    /// this is false: a usable credential may still be there.
    pub fn removed(&self) -> bool {
        self.removed
    }

    /// Takes what the authority needs to be told, leaving this report behind.
    pub fn into_revocation(self) -> Option<Revocation> {
        self.revocation
    }
}

#[derive(Clone)]
pub struct AuthSession {
    inner: Arc<AuthSessionInner>,
}

struct AuthSessionInner {
    commands: Mutex<Option<mpsc::Sender<AuthCommand>>>,
    actor: Mutex<Option<JoinHandle<()>>>,
    /// Where the credential that proves the session lives. Shared with the actor thread
    /// rather than owned by it, so a sign-out removes the record from the same place the
    /// session reads it, from the caller and without waiting for a command.
    store: Arc<dyn CredentialStore>,
    control: Arc<SessionControl>,
    cancellation: CancellationToken,
    actor_completion: Arc<ActorCompletion>,
    authority: Arc<dyn AuthAuthority>,
    /// The application runtime the actor thread is joined on at shutdown.
    runtime: Runtime,
}

struct ActorCompletion {
    finished: AtomicBool,
    notify: Notify,
}

impl ActorCompletion {
    fn new() -> Self {
        Self {
            finished: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

struct ActorCompletionGuard(Arc<ActorCompletion>);

impl Drop for ActorCompletionGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

impl AuthSession {
    /// Starts an OAuth session using the Account capability route from `config`.
    /// A persisted login credential is refreshed once before commands are served.
    pub fn spawn(
        config: ConfigNotification<Config>,
        transport: Arc<TransportHub>,
        runtime: Runtime,
    ) -> Result<Self, AuthError> {
        let (changes, _) = watch::channel(SessionChange::default());
        let control = Arc::new(SessionControl::new(0, 0, changes));
        let account_origin = Url::parse(ACCOUNT_ORIGIN).map_err(|_| AuthError::InvalidResponse)?;
        let authority = Arc::new(OAuthAuthority {
            config,
            transport,
            account_origin,
            runtime: runtime.clone(),
        });
        Self::spawn_with_startup_refresh(
            Arc::new(KeyringCredentialStore),
            control,
            authority,
            runtime,
        )
    }

    pub(super) fn spawn_with_startup_refresh(
        store: Arc<dyn CredentialStore>,
        control: Arc<SessionControl>,
        authority: Arc<dyn AuthAuthority>,
        runtime: Runtime,
    ) -> Result<Self, AuthError> {
        Self::spawn_with_control_and_startup(store, control, authority, true, runtime)
    }

    pub(super) fn spawn_with_control_and_startup(
        store: Arc<dyn CredentialStore>,
        control: Arc<SessionControl>,
        authority: Arc<dyn AuthAuthority>,
        refresh_on_startup: bool,
        runtime: Runtime,
    ) -> Result<Self, AuthError> {
        let (commands, receiver) = mpsc::channel(AUTH_COMMAND_CAPACITY);
        let cancellation = CancellationToken::new();
        let actor_cancellation = cancellation.clone();
        let actor_completion = Arc::new(ActorCompletion::new());
        let completion_guard = ActorCompletionGuard(Arc::clone(&actor_completion));
        let inner = Arc::new(AuthSessionInner {
            commands: Mutex::new(Some(commands)),
            actor: Mutex::new(None),
            store: Arc::clone(&store),
            control: Arc::clone(&control),
            cancellation,
            actor_completion,
            authority: Arc::clone(&authority),
            runtime,
        });
        let actor = thread::Builder::new()
            .name("kytrix-auth-session".to_owned())
            .spawn(move || {
                let _completion = completion_guard;
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(run_auth_session(
                    receiver,
                    store,
                    control,
                    authority,
                    actor_cancellation,
                    refresh_on_startup,
                ));
            })
            .map_err(|_| AuthError::SessionClosed)?;
        *inner
            .actor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(actor);
        Ok(Self { inner })
    }

    pub(super) fn command_sender(&self) -> Result<mpsc::Sender<AuthCommand>, AuthError> {
        if self.inner.cancellation.is_cancelled() {
            return Err(AuthError::SessionClosed);
        }
        self.inner
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
            .ok_or(AuthError::SessionClosed)
    }

    pub(super) async fn send_command<T>(
        &self,
        command: AuthCommand,
        response: oneshot::Receiver<Result<T, AuthError>>,
    ) -> Result<T, AuthError> {
        let sender = self.command_sender()?;
        tokio::select! {
            biased;
            () = self.inner.cancellation.cancelled() => return Err(AuthError::SessionClosed),
            result = sender.send(command) => {
                result.map_err(|_| AuthError::SessionClosed)?;
            }
        }
        tokio::select! {
            biased;
            () = self.inner.cancellation.cancelled() => Err(AuthError::SessionClosed),
            result = response => result.map_err(|_| AuthError::SessionClosed)?,
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.inner.cancellation.cancel();
        let commands = self
            .inner
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(commands);
        let actor = self
            .inner
            .actor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(actor) = actor {
            let _ = self.inner.runtime.blocking(move || actor.join()).await;
        }
        self.inner.actor_completion.wait().await;
    }

    pub fn session_changes(&self) -> watch::Receiver<SessionChange> {
        self.inner.control.subscribe()
    }

    pub(crate) fn prepare_key(&self) -> Result<DpopKey, AuthError> {
        if self.inner.cancellation.is_cancelled() {
            return Err(AuthError::SessionClosed);
        }
        keyring::Entry::store_status()
            .as_ref()
            .map_err(|_| AuthError::CredentialStore)?;
        keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_USER)
            .map_err(|_| AuthError::CredentialStore)?;
        DpopKey::generate()
    }

    pub(crate) async fn exchange_authorization_code(
        &self,
        key: &DpopKey,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<AccountIdentity, AuthError> {
        let response = tokio::select! {
            biased;
            () = self.inner.cancellation.cancelled() => return Err(AuthError::SessionClosed),
            result = self.inner.authority.exchange_authorization_code(
                key,
                code,
                redirect_uri,
                code_verifier,
            ) => result?,
        };
        let account = validate_account(&response)?;
        self.install(response, key.clone()).await?;
        Ok(account)
    }

    pub(crate) async fn current_profile(
        &self,
        expected_user_id: &str,
    ) -> Result<AccountProfile, AuthError> {
        self.profile(expected_user_id).await
    }

    pub(crate) async fn authorization_headers_for(
        &self,
        expected: &AccountProfile,
        method: &str,
        request_url: &str,
    ) -> Result<(String, String), AuthError> {
        self.headers(Some(expected.clone()), method, request_url)
            .await
            .map(|(_, headers)| headers)
    }

    pub(crate) async fn authorization_headers_for_renewal(
        &self,
        expected: &AccountProfile,
        method: &str,
        request_url: &str,
        nonce: &str,
    ) -> Result<(String, String), AuthError> {
        self.renewal_headers(expected.clone(), method, request_url, nonce)
            .await
            .map(|(_, headers)| headers)
    }

    pub(crate) async fn authorization_headers_with_profile(
        &self,
        method: &str,
        request_url: &str,
    ) -> Result<(AccountProfile, (String, String)), AuthError> {
        self.headers(None, method, request_url).await
    }

    pub(crate) async fn local_session_status(&self) -> Result<LocalSessionStatus, AuthError> {
        self.local_status().await
    }

    /// Returns the plan snapshot in the active in-memory session without loading or refreshing it.
    pub async fn current_plan(
        &self,
        expected_user_id: &str,
    ) -> Result<Option<PlanCode>, AuthError> {
        let (respond_to, response_receiver) = oneshot::channel();
        self.send_command(
            AuthCommand::CurrentPlan {
                expected_user_id: expected_user_id.to_owned(),
                respond_to,
            },
            response_receiver,
        )
        .await
    }

    /// Ends this machine's account session and removes the credential that proves it.
    ///
    /// This is a sign-out, not a request: it runs entirely on the caller, it never waits
    /// for the actor thread or for the network, and it succeeds whether or not any of
    /// that is in a usable state. What it cannot do is tell the authority to drop the
    /// refresh token — see [`Self::revoke_session`] for what does.
    pub fn discard_local_session(&self) -> DiscardedSession {
        // Set before anything is removed, and before the credential is read: a refresh
        // that is already in flight holds its own copy of the token, and without this it
        // would write a rotated credential back after the removal — signing the next
        // start in again. Every later command that would refresh is refused by it too.
        self.inner.control.mark_credential_discarded();
        let revocation = match load_credential(&self.inner.store) {
            Ok(LoadedCredential::Active {
                refresh_token, key, ..
            }) => Some(Revocation { refresh_token, key }),
            // A record that grants nothing is nothing to report; one that cannot be read
            // at all is not usable either, so a sign-out does not fail over it.
            Ok(LoadedCredential::RefreshOutcomeUnknown | LoadedCredential::Invalidated)
            | Err(_) => None,
        };

        // The session stops being available here rather than at the actor's next command:
        // the record that proves it is already gone, and every watcher — the menu, the
        // service supervisor — is told so at once.
        self.inner.control.publish_discard();

        // The entry is removed, and the removal is what a sign-out is for. A credential
        // store that refuses to remove it keeps the record it had — which is exactly the
        // one that can sign the user back in — so it is replaced with one that grants
        // nothing before this returns.
        let removed = retire_stored_credential(&self.inner.store);
        DiscardedSession {
            revocation,
            removed,
        }
    }

    /// Reports a finished sign-out to the authority.
    ///
    /// The caller owns this future and is expected to drop its result on the floor:
    /// whether the authority accepted the revocation changes nothing about a sign-out
    /// that already happened here. Telling the authority is a request, so it has to be
    /// driven by a runtime that can make one — an executor without a reactor panics inside
    /// the request instead of sending it.
    pub async fn revoke_session(&self, revocation: Revocation) {
        let Revocation { refresh_token, key } = revocation;
        let result = self.inner.authority.revoke(&refresh_token, &key).await;
        if let Err(error) = result {
            log::warn!("revoking the signed-out refresh token failed: {error}");
        }
    }

    pub(super) async fn install(
        &self,
        response: TokenResponse,
        key: DpopKey,
    ) -> Result<(), AuthError> {
        let (respond_to, response_receiver) = oneshot::channel();
        self.send_command(
            AuthCommand::Install {
                response,
                key,
                respond_to,
            },
            response_receiver,
        )
        .await
    }

    pub(super) async fn profile(
        &self,
        expected_user_id: &str,
    ) -> Result<AccountProfile, AuthError> {
        let (respond_to, response_receiver) = oneshot::channel();
        self.send_command(
            AuthCommand::Profile {
                expected_user_id: expected_user_id.to_owned(),
                respond_to,
            },
            response_receiver,
        )
        .await
    }

    pub(super) async fn headers(
        &self,
        expected: Option<AccountProfile>,
        method: &str,
        request_url: &str,
    ) -> Result<(AccountProfile, (String, String)), AuthError> {
        let (respond_to, response_receiver) = oneshot::channel();
        self.send_command(
            AuthCommand::Headers {
                expected,
                method: method.to_owned(),
                request_url: request_url.to_owned(),
                respond_to,
            },
            response_receiver,
        )
        .await
    }

    pub(super) async fn renewal_headers(
        &self,
        expected: AccountProfile,
        method: &str,
        request_url: &str,
        nonce: &str,
    ) -> Result<(AccountProfile, (String, String)), AuthError> {
        let (respond_to, response_receiver) = oneshot::channel();
        self.send_command(
            AuthCommand::RenewalHeaders {
                expected,
                method: method.to_owned(),
                request_url: request_url.to_owned(),
                nonce: nonce.to_owned(),
                respond_to,
            },
            response_receiver,
        )
        .await
    }

    pub(super) async fn local_status(&self) -> Result<LocalSessionStatus, AuthError> {
        let (respond_to, response_receiver) = oneshot::channel();
        self.send_command(AuthCommand::LocalStatus { respond_to }, response_receiver)
            .await
    }
}

impl Drop for AuthSessionInner {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.commands
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(actor) = self
            .actor
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = actor.join();
        }
    }
}

async fn run_auth_session(
    mut commands: mpsc::Receiver<AuthCommand>,
    store: Arc<dyn CredentialStore>,
    control: Arc<SessionControl>,
    authority: Arc<dyn AuthAuthority>,
    cancellation: CancellationToken,
    refresh_on_startup: bool,
) {
    let mut current = SessionState::Unloaded;
    if refresh_on_startup {
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return,
            result = ensure_session_loaded(
                &mut current,
                &store,
                &control,
                &authority,
                None,
            ) => result,
        };
        if let Err(error) = result
            && !matches!(error, AuthError::MissingCredential)
        {
            log::warn!("startup OAuth refresh failed: {error}");
        }
    }
    loop {
        let command = tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            command = commands.recv() => command,
        };
        let Some(command) = command else { break };
        // A sign-out runs on the caller, so the session it ended is retired here, before
        // the next command is served with it.
        if let Err(error) = control.retire_discarded_session(&mut current)
            && !matches!(error, AuthError::SequenceExhausted)
        {
            log::warn!("failed to retire the signed-out account session: {error}");
        }
        let operation = async {
            match command {
                AuthCommand::Install {
                    response,
                    key,
                    respond_to,
                } => {
                    let result =
                        install_session(&mut current, &store, &control, response, key).await;
                    let _ = respond_to.send(result);
                }
                AuthCommand::Profile {
                    expected_user_id,
                    respond_to,
                } => {
                    let result = current_profile(
                        &mut current,
                        &store,
                        &control,
                        &authority,
                        &expected_user_id,
                    )
                    .await;
                    let _ = respond_to.send(result);
                }
                AuthCommand::CurrentPlan {
                    expected_user_id,
                    respond_to,
                } => {
                    let result = current_plan(&current, &expected_user_id);
                    let _ = respond_to.send(result);
                }
                AuthCommand::Headers {
                    expected,
                    method,
                    request_url,
                    respond_to,
                } => {
                    let result = authorization_headers_inner(
                        &mut current,
                        &store,
                        &control,
                        &authority,
                        AuthorizationProofContext {
                            expected: expected.as_ref(),
                            method: &method,
                            request_url: &request_url,
                            nonce: None,
                            refresh_early_secs: REFRESH_EARLY_SECS,
                        },
                    )
                    .await;
                    let _ = respond_to.send(result);
                }
                AuthCommand::RenewalHeaders {
                    expected,
                    method,
                    request_url,
                    nonce,
                    respond_to,
                } => {
                    let result = authorization_headers_inner(
                        &mut current,
                        &store,
                        &control,
                        &authority,
                        AuthorizationProofContext {
                            expected: Some(&expected),
                            method: &method,
                            request_url: &request_url,
                            nonce: Some(&nonce),
                            refresh_early_secs: RENEWAL_REFRESH_EARLY_SECS,
                        },
                    )
                    .await;
                    let _ = respond_to.send(result);
                }
                AuthCommand::LocalStatus { respond_to } => {
                    let result = local_session_status_inner(&mut current, &store, &control).await;
                    let _ = respond_to.send(result);
                }
            }
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            () = operation => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{SessionAvailability, StoredCredential},
        *,
    };
    use crate::auth::test_support::*;

    mod concurrency {
        use super::*;
        #[tokio::test]
        async fn shutdown_cancels_inflight_authority_work_and_closes_new_commands() {
            let key = DpopKey::generate().expect("test DPoP key should generate");
            let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
                TEST_USER_ID,
                "refresh-token",
                &key,
            ))));
            let authority = Arc::new(StallingRefreshAuthority::new());
            let (changes, _) = watch::channel(SessionChange::default());
            let session = AuthSession::spawn_with_control(
                store.clone(),
                Arc::new(SessionControl::new(0, 0, changes)),
                authority.clone(),
            )
            .expect("test auth actor should start");

            let headers = tokio::spawn({
                let session = session.clone();
                async move {
                    session
                        .headers(None, "GET", "https://api.kytrix.xyz/v1/news")
                        .await
                }
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                authority.started.notified(),
            )
            .await
            .expect("refresh authority should start");

            session.shutdown().await;
            assert!(matches!(headers.await, Ok(Err(AuthError::SessionClosed))));
            assert!(authority.future_dropped.load(Ordering::Acquire));
            let persisted: StoredCredential = serde_json::from_slice(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("shutdown should retain a refresh replay barrier"),
            )
            .expect("the replay barrier should remain valid JSON");
            assert!(matches!(persisted, StoredCredential::RefreshOutcomeUnknown));

            let replacement_authority = Arc::new(FakeRefreshAuthority::new());
            let (replacement, mut replacement_changes) =
                test_session_with_authority(store, 0, 0, replacement_authority.clone());
            assert!(matches!(
                replacement.profile(TEST_USER_ID).await,
                Err(AuthError::RefreshOutcomeUnknown)
            ));
            replacement_changes.changed().await.unwrap();
            assert_eq!(
                replacement_changes.borrow_and_update().availability(),
                SessionAvailability::Unavailable
            );
            assert!(replacement_authority.calls().is_empty());
            replacement.shutdown().await;
            assert!(matches!(
                session.local_session_status().await,
                Err(AuthError::SessionClosed)
            ));

            session.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn cancelled_load_remains_owned_and_is_cached_once() {
            let key = DpopKey::from_signing_key(SigningKey::generate());
            let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
                TEST_USER_ID,
                "refresh-1",
                &key,
            ))));
            store.stall_next();
            let (session, _) = test_session(store.clone(), 0, 0);
            let session = Arc::new(session);
            let request_session = session.clone();
            let request = tokio::spawn(async move { request_session.profile(TEST_USER_ID).await });
            store.started.notified().await;

            request.abort();
            assert!(
                request
                    .await
                    .expect_err("caller should be cancelled")
                    .is_cancelled()
            );
            store.release();

            let profile = session
                .profile(TEST_USER_ID)
                .await
                .expect("the actor should finish the cancelled load");
            assert_eq!(profile.account_id(), TEST_USER_ID);
            assert_eq!(store.loads.load(Ordering::Acquire), 1);
            assert_eq!(store.max_active.load(Ordering::Acquire), 1);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn cancelled_install_commits_store_and_memory_together() {
            let key = DpopKey::from_signing_key(SigningKey::generate());
            let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
                TEST_USER_ID,
                "refresh-1",
                &key,
            ))));
            let (session, _) = test_session(store.clone(), 0, 0);
            let session = Arc::new(session);
            session
                .profile(TEST_USER_ID)
                .await
                .expect("the initial credential should load");
            store.stall_next();
            let request_session = session.clone();
            let request = tokio::spawn(async move {
                request_session
                    .install(
                        token_response(TEST_USER_ID, "refresh-2", "access-2"),
                        DpopKey::generate().expect("test DPoP key should generate"),
                    )
                    .await
            });
            store.started.notified().await;

            request.abort();
            assert!(
                request
                    .await
                    .expect_err("caller should be cancelled")
                    .is_cancelled()
            );
            store.release();

            let (_, (access_token, _)) = session
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await
                .expect("the cancelled install should commit the active session");
            assert_eq!(access_token, "access-2");
            let stored = stored_active_credential(&store);
            assert_eq!(stored.1, "refresh-2");
            assert_eq!(store.max_active.load(Ordering::Acquire), 1);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn last_owner_drop_waits_for_the_active_install_commit() {
            let key = DpopKey::from_signing_key(SigningKey::generate());
            let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
                TEST_USER_ID,
                "refresh-1",
                &key,
            ))));
            let (session, _) = test_session(store.clone(), 0, 0);
            let session = Arc::new(session);
            session
                .profile(TEST_USER_ID)
                .await
                .expect("the initial credential should load");
            store.stall_next();
            let request_session = session.clone();
            let request = tokio::spawn(async move {
                request_session
                    .install(
                        token_response(TEST_USER_ID, "refresh-2", "access-2"),
                        DpopKey::generate().expect("test DPoP key should generate"),
                    )
                    .await
            });
            store.started.notified().await;
            request.abort();
            assert!(
                request
                    .await
                    .expect_err("caller should be cancelled")
                    .is_cancelled()
            );

            let runtime = session.inner.runtime.clone();
            let mut drop_task = runtime.blocking(move || drop(session));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), &mut drop_task)
                    .await
                    .is_err(),
                "the last owner must join the active store operation"
            );
            store.release();
            drop_task
                .await
                .expect("the last owner should join after the store releases");

            let stored = stored_active_credential(&store);
            assert_eq!(stored.1, "refresh-2");
            assert_eq!(store.active.load(Ordering::Acquire), 0);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stalled_store_allows_only_one_queued_auth_command_during_install() {
            let key = DpopKey::from_signing_key(SigningKey::generate());
            let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
                TEST_USER_ID,
                "refresh-1",
                &key,
            ))));
            let (session, _) = test_session(store.clone(), 0, 0);
            let session = Arc::new(session);
            session
                .profile(TEST_USER_ID)
                .await
                .expect("the initial credential should load");
            store.stall_next();
            let request_session = session.clone();
            let active = tokio::spawn(async move {
                request_session
                    .install(
                        token_response(TEST_USER_ID, "refresh-2", "access-2"),
                        DpopKey::generate().expect("test DPoP key should generate"),
                    )
                    .await
            });
            store.started.notified().await;

            let (queued_response, queued_receiver) = oneshot::channel();
            session
                .command_sender()
                .expect("the auth actor should remain open")
                .try_send(AuthCommand::LocalStatus {
                    respond_to: queued_response,
                })
                .expect("one command should fit behind the active store operation");
            let (overflow_response, _overflow_receiver) = oneshot::channel();
            assert!(matches!(
                session
                    .command_sender()
                    .expect("the auth actor should remain open")
                    .try_send(AuthCommand::LocalStatus {
                        respond_to: overflow_response,
                    }),
                Err(mpsc::error::TrySendError::Full(_))
            ));

            store.release();
            active
                .await
                .expect("the active caller should remain joined")
                .expect("the active refresh should complete");
            assert!(
                queued_receiver
                    .await
                    .expect("the queued response should arrive")
                    .is_ok()
            );
            assert_eq!(store.max_active.load(Ordering::Acquire), 1);
        }
    }

    #[tokio::test]
    async fn current_plan_should_read_only_the_active_in_memory_session() {
        let store = Arc::new(InMemoryCredentialStore::default());
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, _) = test_session_with_authority(store, 0, 0, Arc::clone(&authority));

        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);
        assert!(authority.calls().is_empty());

        session.shutdown().await;
    }

    #[tokio::test]
    async fn startup_refreshes_a_persisted_credential_before_the_first_command() {
        let key = DpopKey::generate().unwrap();
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-startup",
            &key,
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (changes, mut change_receiver) = watch::channel(SessionChange::default());
        let control = Arc::new(SessionControl::new(0, 0, changes));
        let session = AuthSession::spawn_with_startup_refresh(
            store,
            control,
            authority.clone(),
            Runtime::for_test(),
        )
        .expect("the auth actor should start");

        let profile = session
            .profile(TEST_USER_ID)
            .await
            .expect("the persisted credential should be refreshed at startup");

        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-startup".to_owned())]
        );
        assert_eq!(profile.credential_revision(), 1);
        assert_eq!(profile.credential_epoch(), 1);
        change_receiver
            .changed()
            .await
            .expect("startup refresh should publish a session change");
        assert_eq!(
            change_receiver.borrow_and_update().availability(),
            SessionAvailability::Available
        );

        session.shutdown().await;
    }

    #[tokio::test]
    async fn install_and_refresh_should_update_the_in_memory_plan_without_persisting_it() {
        let store = Arc::new(InMemoryCredentialStore::default());
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, _) = test_session_with_authority(store.clone(), 0, 0, Arc::clone(&authority));
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "access-1",
                    0,
                    PlanCode::Plus,
                ),
                DpopKey::generate().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            session.current_plan(TEST_USER_ID).await.unwrap(),
            Some(PlanCode::Plus)
        );

        authority.plan_next(PlanCode::Pro);
        session
            .headers(None, "GET", "https://api.kytrix.xyz/v1/news")
            .await
            .unwrap();
        assert_eq!(
            session.current_plan(TEST_USER_ID).await.unwrap(),
            Some(PlanCode::Pro)
        );

        let payload = store
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        let persisted = serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
        assert!(persisted.get("plan_code").is_none());
        assert!(persisted.get("kytrix_plan_code").is_none());

        session.shutdown().await;
    }

    #[tokio::test]
    async fn current_plan_should_reject_another_user_and_clear_after_invalidation_or_sign_out() {
        let store = Arc::new(InMemoryCredentialStore::default());
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, _) = test_session_with_authority(store, 0, 0, Arc::clone(&authority));
        session
            .install(
                token_response_with_plan(TEST_USER_ID, "refresh-1", "access-1", PlanCode::Pro),
                DpopKey::generate().unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            session.current_plan(TEST_OTHER_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        session.discard_local_session();
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);

        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-2",
                    "access-2",
                    0,
                    PlanCode::Plus,
                ),
                DpopKey::generate().unwrap(),
            )
            .await
            .unwrap();
        authority.fail_next(AuthError::InvalidGrant);
        assert!(matches!(
            session
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::InvalidGrant)
        ));
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);

        session.shutdown().await;
    }

    #[tokio::test]
    async fn renewal_headers_rotate_tokens_without_changing_the_session_profile() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, changes, _) =
            test_session_with_authority_and_control(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "access-1",
                    RENEWAL_REFRESH_EARLY_SECS,
                    PlanCode::Free,
                ),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .unwrap();
        let profile = session.profile(TEST_USER_ID).await.unwrap();
        let committed_change = *changes.borrow();
        authority.rotate_next("refresh-2");

        let (renewed_profile, (access_token, proof)) = session
            .renewal_headers(
                profile.clone(),
                "GET",
                "https://api.kytrix.xyz/",
                "challenge-nonce",
            )
            .await
            .unwrap();

        assert_eq!(renewed_profile, profile);
        assert_eq!(access_token, "access-1");
        assert_eq!(*changes.borrow(), committed_change);
        assert_eq!(stored_active_credential(&store).1, "refresh-2");
        assert_eq!(authority.calls().len(), 1);
        let claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(proof.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["nonce"], "challenge-nonce");
        assert_eq!(
            claims["ath"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"access-1"))
        );
    }

    #[tokio::test]
    async fn renewal_headers_refresh_when_local_token_has_sixty_one_seconds_remaining() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, _) = test_session_with_authority(store, 0, 0, authority.clone());
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "initial-access",
                    61,
                    PlanCode::Free,
                ),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .unwrap();
        let profile = session.profile(TEST_USER_ID).await.unwrap();

        let (_, (access_token, _)) = session
            .renewal_headers(profile, "GET", "https://api.kytrix.xyz/", "challenge-nonce")
            .await
            .unwrap();

        assert_eq!(access_token, "access-1");
        assert_eq!(authority.calls().len(), 1);
    }

    #[tokio::test]
    async fn renewal_headers_reuse_a_fresh_access_token_with_a_new_nonce_proof() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, _) = test_session_with_authority(store, 0, 0, authority.clone());
        session
            .install(
                token_response(TEST_USER_ID, "refresh-1", "fresh-access"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .unwrap();
        let profile = session.profile(TEST_USER_ID).await.unwrap();

        let (_, (access_token, first)) = session
            .renewal_headers(
                profile.clone(),
                "GET",
                "https://api.kytrix.xyz/",
                "first-nonce",
            )
            .await
            .unwrap();
        let (_, (_, second)) = session
            .renewal_headers(profile, "GET", "https://api.kytrix.xyz/", "second-nonce")
            .await
            .unwrap();

        assert_eq!(access_token, "fresh-access");
        assert_ne!(first, second);
        assert!(authority.calls().is_empty());
    }

    #[tokio::test]
    async fn revision_exhaustion_clears_the_session_and_is_permanent() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let (session, mut changes) = test_session(store.clone(), u64::MAX - 1, 0);
        let old_profile = session
            .profile(TEST_USER_ID)
            .await
            .expect("the maximum revision should install once");
        changes.changed().await.unwrap();
        assert_eq!(old_profile.credential_revision(), u64::MAX);
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Available
        );

        let result = session
            .install(
                token_response(TEST_REPLACEMENT_USER_ID, "refresh-2", "access-2"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await;
        assert!(matches!(result, Err(AuthError::SequenceExhausted)));
        changes.changed().await.unwrap();
        let exhausted = *changes.borrow_and_update();
        assert!(exhausted.is_exhausted());
        assert_eq!(exhausted.availability(), SessionAvailability::Unavailable);
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::SequenceExhausted)
        ));
        assert!(matches!(
            session
                .headers(Some(old_profile), "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::SequenceExhausted)
        ));
        // A sign-out is not a command, so an exhausted sequence does not refuse it: the
        // credential here is removed whether or not the session was still usable.
        assert!(session.discard_local_session().removed());
        assert!(matches!(
            session
                .install(
                    token_response(TEST_REPLACEMENT_USER_ID, "refresh-4", "access-4"),
                    DpopKey::from_signing_key(SigningKey::generate()),
                )
                .await,
            Err(AuthError::SequenceExhausted)
        ));
        assert!(matches!(
            session
                .install(
                    token_response(TEST_REPLACEMENT_USER_ID, "refresh-3", "access-3"),
                    DpopKey::from_signing_key(SigningKey::generate()),
                )
                .await,
            Err(AuthError::SequenceExhausted)
        ));
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);
        // The sign-out reads the record to know which token the authority still holds, and
        // removes it: the load and the removal are its own, on top of the session's.
        assert_eq!(store.loads.load(Ordering::Acquire), 2);
        assert_eq!(store.stores.load(Ordering::Acquire), 2);
        assert_eq!(store.deletes.load(Ordering::Acquire), 1);
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn epoch_exhaustion_clears_the_session_and_is_permanent() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let (session, mut changes) = test_session(store.clone(), 0, u64::MAX - 1);
        let old_profile = session
            .profile(TEST_USER_ID)
            .await
            .expect("the maximum epoch should install once");
        changes.changed().await.unwrap();
        assert_eq!(old_profile.credential_epoch(), u64::MAX);
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Available
        );

        let result = session
            .install(
                token_response(TEST_REPLACEMENT_USER_ID, "refresh-2", "access-2"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await;
        assert!(matches!(result, Err(AuthError::SequenceExhausted)));
        changes.changed().await.unwrap();
        assert!(changes.borrow_and_update().is_exhausted());
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::SequenceExhausted)
        ));
        assert!(matches!(
            session
                .headers(Some(old_profile), "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::SequenceExhausted)
        ));
        assert!(matches!(
            session
                .install(
                    token_response(TEST_REPLACEMENT_USER_ID, "refresh-3", "access-3"),
                    DpopKey::from_signing_key(SigningKey::generate()),
                )
                .await,
            Err(AuthError::SequenceExhausted)
        ));
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);
        assert_eq!(store.loads.load(Ordering::Acquire), 1);
        assert_eq!(store.stores.load(Ordering::Acquire), 2);
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalidation_epoch_exhaustion_publishes_terminal_before_tombstone_store() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        authority.fail_next(AuthError::InvalidGrant);
        store.stall_next_store();
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, u64::MAX, authority);
        let session = Arc::new(session);
        let request_session = session.clone();
        let request = tokio::spawn(async move { request_session.profile(TEST_USER_ID).await });

        store.started.notified().await;
        changes.changed().await.unwrap();
        let change = *changes.borrow_and_update();
        assert!(change.is_exhausted());
        assert_eq!(change.availability(), SessionAvailability::Unavailable);
        assert!(!request.is_finished());

        store.release();
        assert!(matches!(
            request.await.expect("the auth command should finish"),
            Err(AuthError::SequenceExhausted)
        ));
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::SequenceExhausted)
        ));
        assert!(!changes.has_changed().unwrap());
    }
}

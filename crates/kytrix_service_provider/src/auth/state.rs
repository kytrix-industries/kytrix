use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use tokio::sync::{Notify, watch};

use super::{
    AccessToken, AuthAuthority, AuthError, AuthorizationProofContext, CredentialStore, DpopKey,
    LoadedCredential, LocalSessionStatus, PlanCode, RefreshedCredential, Session,
    SessionAvailability, SessionChange, SessionState, TokenResponse, canonical_user_id,
    load_credential, store_credential, store_invalidated_credential, store_refresh_outcome_unknown,
};
use crate::account_profile::AccountProfile;

pub(super) struct SessionControl {
    revision: AtomicU64,
    epoch: AtomicU64,
    /// Whether the credential on this machine has been discarded by a sign-out.
    ///
    /// A refresh that was already in flight when the user signed out still holds the
    /// token it loaded, so it would otherwise write a rotated credential back after the
    /// sign-out deleted the old one — signing the user back in on the next start. While
    /// this is set, nothing may persist a credential; the next login is what clears it.
    discarded: AtomicBool,
    /// Wakes the work a sign-out interrupts, so a command that is waiting for the
    /// authority stops waiting for an answer it can no longer commit.
    signed_out: Notify,
    changes: watch::Sender<SessionChange>,
}

impl SessionControl {
    pub(super) fn new(revision: u64, epoch: u64, changes: watch::Sender<SessionChange>) -> Self {
        Self {
            revision: AtomicU64::new(revision),
            epoch: AtomicU64::new(epoch),
            discarded: AtomicBool::new(false),
            signed_out: Notify::new(),
            changes,
        }
    }

    /// Records that the local credential is gone, and refuses every later write.
    pub(super) fn mark_credential_discarded(&self) {
        self.discarded.store(true, Ordering::Release);
        self.signed_out.notify_waiters();
    }

    /// Lets the login that has just finished persist the credential it carries.
    ///
    /// A sign-out refuses every write the session it ended would make, so that a refresh
    /// still in flight cannot put the record back. A login is not that session: it is the
    /// user asking for a new one, and the actor serves one command at a time, so nothing
    /// of the signed-out session can still be running here.
    pub(super) fn begin_new_session(&self) {
        self.discarded.store(false, Ordering::Release);
    }

    /// Waits for the sign-out that ends the session an in-flight command belongs to.
    ///
    /// A sign-out runs on the caller while a command is already waiting for the authority;
    /// this is what lets that command stop waiting for an answer it can no longer commit.
    pub(super) async fn wait_for_sign_out(&self) {
        loop {
            let notified = self.signed_out.notified();
            if self.discarded.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// Refuses a credential the session is no longer entitled to persist.
    ///
    /// A sign-out sets this until the next login clears it: a credential that was removed
    /// by the user is not one the session it was removed from may put back.
    pub(super) fn credential_is_current(&self) -> Result<(), AuthError> {
        if self.discarded.load(Ordering::Acquire) {
            Err(AuthError::SessionClosed)
        } else {
            Ok(())
        }
    }

    pub(super) fn next_revision(&self) -> Result<u64, AuthError> {
        next_monotonic(&self.revision)
    }

    pub(super) fn next_change(
        &self,
        availability: SessionAvailability,
    ) -> Result<SessionChange, AuthError> {
        next_session_change_with(&self.epoch, availability)
    }

    pub(super) fn publish(&self, change: SessionChange) {
        self.changes.send_replace(change);
    }

    /// Says the session is over because its credential was signed out and removed.
    ///
    /// The availability published is the one every watcher already treats as a sign-out,
    /// so nothing has to learn a second way of being told. A sequence that has already run
    /// out is left as it is: its change is the last thing watchers are ever told, and a
    /// sign-out cannot make a session identity available again.
    pub(super) fn publish_discard(&self) {
        if self.changes.borrow().is_exhausted() {
            return;
        }
        if let Ok(change) = self.next_change(SessionAvailability::Unavailable) {
            self.publish(change);
        }
    }

    /// Retires the session behind a credential that was signed out.
    ///
    /// The sign-out itself runs on the caller and cannot touch this state, so the actor
    /// gives up on it here — before any command is served with a session whose credential
    /// the user has already removed.
    pub(super) fn retire_discarded_session(
        &self,
        current_session: &mut SessionState,
    ) -> Result<(), AuthError> {
        if self.credential_is_current().is_ok() {
            return Ok(());
        }
        transition_to_invalidated(current_session, self).map(|_| ())
    }

    pub(super) fn publish_exhaustion(&self) {
        publish_session_exhaustion_to(&self.changes);
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<SessionChange> {
        self.changes.subscribe()
    }
}

pub(super) fn exhaust_session(
    current_session: &mut SessionState,
    control: &SessionControl,
) -> AuthError {
    if !matches!(current_session, SessionState::Exhausted) {
        *current_session = SessionState::Exhausted;
        control.publish_exhaustion();
    }
    AuthError::SequenceExhausted
}

pub(super) fn next_change_or_exhaust(
    current_session: &mut SessionState,
    control: &SessionControl,
    availability: SessionAvailability,
) -> Result<SessionChange, AuthError> {
    match control.next_change(availability) {
        Ok(change) => Ok(change),
        Err(AuthError::SequenceExhausted) => Err(exhaust_session(current_session, control)),
        Err(error) => Err(error),
    }
}

pub(super) fn allocate_session_identity(
    current_session: &mut SessionState,
    control: &SessionControl,
) -> Result<(u64, u64, SessionChange), AuthError> {
    let revision = match control.next_revision() {
        Ok(revision) => revision,
        Err(AuthError::SequenceExhausted) => {
            return Err(exhaust_session(current_session, control));
        }
        Err(error) => return Err(error),
    };
    let change = next_change_or_exhaust(current_session, control, SessionAvailability::Available)?;
    let Some(credential_epoch) = change.epoch() else {
        return Err(exhaust_session(current_session, control));
    };
    Ok((revision, credential_epoch, change))
}

pub(super) fn transition_to_invalidated(
    current_session: &mut SessionState,
    control: &SessionControl,
) -> Result<bool, AuthError> {
    match current_session {
        SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
        SessionState::Invalidated => return Ok(false),
        SessionState::Unloaded | SessionState::Active(_) | SessionState::RefreshOutcomeUnknown => {}
    }
    let change =
        next_change_or_exhaust(current_session, control, SessionAvailability::Unavailable)?;
    *current_session = SessionState::Invalidated;
    control.publish(change);
    Ok(true)
}

pub(super) fn transition_to_refresh_outcome_unknown(
    current_session: &mut SessionState,
    control: &SessionControl,
) -> Result<(), AuthError> {
    match current_session {
        SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
        SessionState::RefreshOutcomeUnknown => return Ok(()),
        SessionState::Unloaded | SessionState::Active(_) | SessionState::Invalidated => {}
    }
    let change =
        next_change_or_exhaust(current_session, control, SessionAvailability::Unavailable)?;
    *current_session = SessionState::RefreshOutcomeUnknown;
    control.publish(change);
    Ok(())
}

pub(super) async fn invalidate_credential(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    cause: AuthError,
) -> AuthError {
    match current_session {
        SessionState::Exhausted => return AuthError::SequenceExhausted,
        SessionState::Invalidated => return cause,
        SessionState::Unloaded | SessionState::Active(_) | SessionState::RefreshOutcomeUnknown => {}
    }
    transition_and_persist_invalidation(current_session, store, control)
        .await
        .err()
        .unwrap_or(cause)
}

pub(super) async fn invalidate_after_confirmed_rotation(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    cause: AuthError,
) -> AuthError {
    match transition_and_persist_invalidation(current_session, store, control).await {
        Err(AuthError::SequenceExhausted) => AuthError::SequenceExhausted,
        Ok(()) | Err(_) => cause,
    }
}

pub(super) async fn transition_and_persist_invalidation(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
) -> Result<(), AuthError> {
    let transition = transition_to_invalidated(current_session, control).map(|_| ());
    let persistence = store_invalidated_credential(store);
    transition?;
    persistence
}
pub(super) async fn local_session_status_inner(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
) -> Result<LocalSessionStatus, AuthError> {
    let availability = match current_session {
        SessionState::Active(_) => SessionAvailability::Available,
        SessionState::RefreshOutcomeUnknown
        | SessionState::Invalidated
        | SessionState::Exhausted => SessionAvailability::Unavailable,
        SessionState::Unloaded => match load_credential(store) {
            Ok(LoadedCredential::Active { .. }) => SessionAvailability::Unknown,
            Ok(LoadedCredential::RefreshOutcomeUnknown) => {
                *current_session = SessionState::RefreshOutcomeUnknown;
                SessionAvailability::Unavailable
            }
            Ok(LoadedCredential::Invalidated) => {
                *current_session = SessionState::Invalidated;
                SessionAvailability::Unavailable
            }
            Err(AuthError::MissingCredential) => SessionAvailability::Unknown,
            Err(AuthError::InvalidCredential) => {
                *current_session = SessionState::Invalidated;
                let _ = store_invalidated_credential(store);
                SessionAvailability::Unavailable
            }
            Err(error) => return Err(error),
        },
    };
    Ok(LocalSessionStatus::new(
        *control.changes.borrow(),
        availability,
    ))
}

pub(super) async fn ensure_session_loaded(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    authority: &Arc<dyn AuthAuthority>,
    expected_user_id: Option<&str>,
) -> Result<(), AuthError> {
    match current_session {
        SessionState::Active(current) => {
            return validate_expected_user_id(&current.user_id, expected_user_id);
        }
        SessionState::Invalidated => return Err(AuthError::InvalidCredential),
        SessionState::RefreshOutcomeUnknown => return Err(AuthError::RefreshOutcomeUnknown),
        SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
        SessionState::Unloaded => {}
    }
    let (user_id, refresh_token, key) = match load_credential(store) {
        Ok(LoadedCredential::Invalidated) => {
            *current_session = SessionState::Invalidated;
            return Err(AuthError::InvalidCredential);
        }
        Ok(LoadedCredential::RefreshOutcomeUnknown) => {
            transition_to_refresh_outcome_unknown(current_session, control)?;
            return Err(AuthError::RefreshOutcomeUnknown);
        }
        Ok(LoadedCredential::Active {
            user_id,
            refresh_token,
            key,
        }) => {
            validate_expected_user_id(&user_id, expected_user_id)?;
            (user_id, refresh_token, key)
        }
        Err(error @ AuthError::InvalidCredential) => {
            return Err(invalidate_credential(current_session, store, control, error).await);
        }
        Err(error) => return Err(error),
    };
    let refreshed = match refresh_with_replay_barrier(
        store,
        control,
        authority,
        &user_id,
        &refresh_token,
        &key,
    )
    .await
    {
        Ok(refreshed) => refreshed,
        Err(error @ AuthError::InvalidGrant) => {
            return Err(invalidate_credential(current_session, store, control, error).await);
        }
        Err(AuthError::RefreshOutcomeUnknown) => {
            transition_to_refresh_outcome_unknown(current_session, control)?;
            return Err(AuthError::RefreshOutcomeUnknown);
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = store_credential(store, control, &user_id, &refreshed.refresh_token, &key) {
        return Err(
            invalidate_after_confirmed_rotation(current_session, store, control, error).await,
        );
    }
    let (revision, credential_epoch, change) = allocate_session_identity(current_session, control)?;
    *current_session = SessionState::Active(Box::new(Session {
        user_id,
        plan_code: refreshed.plan_code,
        revision,
        credential_epoch,
        refresh_token: Some(refreshed.refresh_token),
        key,
        access: Some(refreshed.access),
    }));
    control.publish(change);
    Ok(())
}

pub(super) async fn refresh_with_replay_barrier(
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    authority: &Arc<dyn AuthAuthority>,
    user_id: &str,
    refresh_token: &str,
    key: &DpopKey,
) -> Result<RefreshedCredential, AuthError> {
    store_refresh_outcome_unknown(store, control)?;
    let refreshed = tokio::select! {
        biased;
        // The sign-out removed the record this refresh belongs to while the request was
        // out: there is nothing left to commit the answer to, so the wait ends here rather
        // than holding the session until a response it cannot use comes back.
        () = control.wait_for_sign_out() => return Err(AuthError::SessionClosed),
        result = authority.refresh(user_id, refresh_token, key) => result,
    };
    match refreshed {
        Ok(refreshed) => Ok(refreshed),
        Err(AuthError::InvalidGrant) => Err(AuthError::InvalidGrant),
        Err(error) if refresh_failure_is_replay_safe(&error) => {
            store_credential(store, control, user_id, refresh_token, key)
                .map_err(|_| AuthError::RefreshOutcomeUnknown)?;
            Err(error)
        }
        Err(_) => Err(AuthError::RefreshOutcomeUnknown),
    }
}

pub(super) fn refresh_failure_is_replay_safe(error: &AuthError) -> bool {
    matches!(
        error,
        AuthError::Request
            | AuthError::HttpClient
            | AuthError::Route
            | AuthError::SessionClosed
            | AuthError::SecureRandom
    ) || matches!(error, AuthError::Status(status) if !status.is_server_error())
}

pub(super) fn validate_expected_user_id(
    actual_user_id: &str,
    expected_user_id: Option<&str>,
) -> Result<(), AuthError> {
    if expected_user_id
        .is_some_and(|expected| !canonical_user_id(expected) || expected != actual_user_id)
    {
        return Err(AuthError::InvalidCredential);
    }
    Ok(())
}

pub(super) async fn authorization_headers_inner(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    authority: &Arc<dyn AuthAuthority>,
    context: AuthorizationProofContext<'_>,
) -> Result<(AccountProfile, (String, String)), AuthError> {
    ensure_session_loaded(
        current_session,
        store,
        control,
        authority,
        context.expected.map(AccountProfile::account_id),
    )
    .await?;
    let current_profile = match current_session {
        SessionState::Active(current) => current.profile(),
        SessionState::Unloaded => return Err(AuthError::MissingCredential),
        SessionState::RefreshOutcomeUnknown => return Err(AuthError::RefreshOutcomeUnknown),
        SessionState::Invalidated => return Err(AuthError::InvalidCredential),
        SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
    };
    if context
        .expected
        .is_some_and(|expected| current_profile != *expected)
    {
        return Err(AuthError::InvalidCredential);
    }
    if matches!(
        current_session,
        SessionState::Active(current) if current.refresh_token.is_none()
    ) {
        return Err(AuthError::RefreshOutcomeUnknown);
    }
    let now = timestamp()?;
    let should_refresh = match current_session {
        SessionState::Active(current) => current.access.as_ref().is_none_or(|access| {
            access.expires_at <= now.saturating_add(context.refresh_early_secs)
        }),
        SessionState::Unloaded => return Err(AuthError::MissingCredential),
        SessionState::RefreshOutcomeUnknown => return Err(AuthError::RefreshOutcomeUnknown),
        SessionState::Invalidated => return Err(AuthError::InvalidCredential),
        SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
    };
    if should_refresh {
        let (user_id, refresh_token, key) = match current_session {
            SessionState::Active(current) => (
                current.user_id.clone(),
                current
                    .refresh_token
                    .clone()
                    .ok_or(AuthError::RefreshOutcomeUnknown)?,
                current.key.clone(),
            ),
            SessionState::Unloaded => return Err(AuthError::MissingCredential),
            SessionState::RefreshOutcomeUnknown => {
                return Err(AuthError::RefreshOutcomeUnknown);
            }
            SessionState::Invalidated => return Err(AuthError::InvalidCredential),
            SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
        };
        let refreshed =
            refresh_with_replay_barrier(store, control, authority, &user_id, &refresh_token, &key)
                .await;
        let refreshed = match refreshed {
            Ok(refreshed) => refreshed,
            Err(error @ AuthError::InvalidGrant) => {
                return Err(invalidate_credential(current_session, store, control, error).await);
            }
            Err(AuthError::RefreshOutcomeUnknown) => {
                transition_to_refresh_outcome_unknown(current_session, control)?;
                return Err(AuthError::RefreshOutcomeUnknown);
            }
            Err(error) => return Err(error),
        };
        let persistence = match current_session {
            SessionState::Active(current) => {
                commit_refresh(
                    current,
                    store,
                    control,
                    refreshed.refresh_token,
                    refreshed.access,
                    refreshed.plan_code,
                )
                .await
            }
            SessionState::Unloaded => return Err(AuthError::MissingCredential),
            SessionState::RefreshOutcomeUnknown => {
                return Err(AuthError::RefreshOutcomeUnknown);
            }
            SessionState::Invalidated => return Err(AuthError::InvalidCredential),
            SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
        };
        if let Err(error) = persistence {
            return Err(invalidate_after_confirmed_rotation(
                current_session,
                store,
                control,
                error,
            )
            .await);
        }
    }
    let current = match current_session {
        SessionState::Active(current) => current,
        SessionState::Unloaded => return Err(AuthError::MissingCredential),
        SessionState::RefreshOutcomeUnknown => return Err(AuthError::RefreshOutcomeUnknown),
        SessionState::Invalidated => return Err(AuthError::InvalidCredential),
        SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
    };
    let access = current.access.as_ref().ok_or(AuthError::InvalidResponse)?;
    let proof = current.key.proof(
        context.method,
        context.request_url,
        Some(&access.value),
        context.nonce,
    )?;
    let profile = current.profile();
    if context
        .expected
        .is_some_and(|expected| profile != *expected)
    {
        return Err(AuthError::InvalidCredential);
    }
    Ok((profile, (access.value.clone(), proof)))
}

pub(super) async fn current_profile(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    authority: &Arc<dyn AuthAuthority>,
    expected_user_id: &str,
) -> Result<AccountProfile, AuthError> {
    ensure_session_loaded(
        current_session,
        store,
        control,
        authority,
        Some(expected_user_id),
    )
    .await?;
    let current = match current_session {
        SessionState::Active(current) => current,
        SessionState::Unloaded => return Err(AuthError::MissingCredential),
        SessionState::RefreshOutcomeUnknown => return Err(AuthError::RefreshOutcomeUnknown),
        SessionState::Invalidated => return Err(AuthError::InvalidCredential),
        SessionState::Exhausted => return Err(AuthError::SequenceExhausted),
    };
    Ok(current.profile())
}

pub(super) fn current_plan(
    current_session: &SessionState,
    expected_user_id: &str,
) -> Result<Option<PlanCode>, AuthError> {
    let SessionState::Active(current) = current_session else {
        return Ok(None);
    };
    validate_expected_user_id(&current.user_id, Some(expected_user_id))?;
    Ok(Some(current.plan_code))
}

pub(super) async fn install_session(
    current_session: &mut SessionState,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    response: TokenResponse,
    key: DpopKey,
) -> Result<(), AuthError> {
    if matches!(current_session, SessionState::Exhausted) {
        return Err(AuthError::SequenceExhausted);
    }
    let expires_at = timestamp()?
        .checked_add(response.expires_in)
        .ok_or(AuthError::InvalidResponse)?;
    let (revision, credential_epoch, change) = allocate_session_identity(current_session, control)?;
    // The login the caller just completed is the user asking for a session again: the
    // record it carries is a new one, not the one a sign-out refused to let anything
    // write back.
    control.begin_new_session();
    store_credential(
        store,
        control,
        &response.kytrix_user_id,
        &response.refresh_token,
        &key,
    )?;
    *current_session = SessionState::Active(Box::new(Session {
        user_id: response.kytrix_user_id,
        plan_code: response.kytrix_plan_code,
        revision,
        credential_epoch,
        refresh_token: Some(response.refresh_token),
        key,
        access: Some(AccessToken {
            value: response.access_token,
            expires_at,
        }),
    }));
    control.publish(change);
    Ok(())
}

pub(super) async fn commit_refresh(
    session: &mut Session,
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    refresh_token: String,
    access: AccessToken,
    plan_code: PlanCode,
) -> Result<(), AuthError> {
    store_credential(
        store,
        control,
        &session.user_id,
        &refresh_token,
        &session.key,
    )?;
    session.refresh_token = Some(refresh_token);
    session.access = Some(access);
    session.plan_code = plan_code;
    Ok(())
}

pub(super) fn publish_session_exhaustion_to(sender: &watch::Sender<SessionChange>) {
    sender.send_if_modified(|change| {
        if change.is_exhausted() {
            false
        } else {
            *change = SessionChange::exhausted();
            true
        }
    });
}

impl Session {
    pub(super) fn profile(&self) -> AccountProfile {
        AccountProfile::new(&self.user_id, self.revision, self.credential_epoch)
    }
}

pub(super) fn next_session_change_with(
    counter: &AtomicU64,
    availability: SessionAvailability,
) -> Result<SessionChange, AuthError> {
    next_monotonic(counter).map(|epoch| SessionChange::published(epoch, availability))
}

pub(super) fn next_monotonic(counter: &AtomicU64) -> Result<u64, AuthError> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|_| AuthError::SequenceExhausted)
}

pub(super) fn timestamp() -> Result<i64, AuthError> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AuthError::SystemTime)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| AuthError::SystemTime)
}

#[cfg(test)]
mod tests {
    use super::{
        super::{RENEWAL_REFRESH_EARLY_SECS, StoredCredential},
        *,
    };
    use crate::auth::test_support::*;

    #[tokio::test]
    async fn production_refresh_5xx_is_persisted_unknown_and_never_replayed() {
        let mut server = TestOAuthServer::spawn(vec![OAuthHttpBehavior::Respond {
            status: 502,
            body: serde_json::json!({ "error": "temporarily_unavailable" }).to_string(),
        }])
        .await;
        let transport = test_transport();
        let key = DpopKey::generate().unwrap();
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-once",
            &key,
        ))));
        let authority = oauth_authority(server.origin.clone(), Arc::clone(&transport));
        let (session, mut changes) = {
            let (sender, receiver) = watch::channel(SessionChange::default());
            (
                AuthSession::spawn_with_control(
                    store.clone(),
                    Arc::new(SessionControl::new(0, 0, sender)),
                    authority,
                )
                .unwrap(),
                receiver,
            )
        };

        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::RefreshOutcomeUnknown)
        ));
        let request = server.request().await;
        assert_eq!(
            form_fields(&request.body).get("refresh_token"),
            Some(&"refresh-once".to_owned())
        );
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::RefreshOutcomeUnknown)
        ));
        session.shutdown().await;

        let replacement_authority = oauth_authority(server.origin.clone(), Arc::clone(&transport));
        let (sender, mut replacement_changes) = watch::channel(SessionChange::default());
        let replacement = AuthSession::spawn_with_control(
            store,
            Arc::new(SessionControl::new(0, 0, sender)),
            replacement_authority,
        )
        .unwrap();
        assert!(matches!(
            replacement.profile(TEST_USER_ID).await,
            Err(AuthError::RefreshOutcomeUnknown)
        ));
        replacement_changes.changed().await.unwrap();
        assert!(server.requests.try_recv().is_err());

        replacement.shutdown().await;
        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn production_refresh_shutdown_after_submission_keeps_the_replay_barrier() {
        let mut server = TestOAuthServer::spawn(vec![OAuthHttpBehavior::Stall]).await;
        let transport = test_transport();
        let key = DpopKey::generate().unwrap();
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-once",
            &key,
        ))));
        let authority = oauth_authority(server.origin.clone(), Arc::clone(&transport));
        let (sender, _) = watch::channel(SessionChange::default());
        let session = AuthSession::spawn_with_control(
            store.clone(),
            Arc::new(SessionControl::new(0, 0, sender)),
            authority,
        )
        .unwrap();
        let request_session = session.clone();
        let request = tokio::spawn(async move { request_session.profile(TEST_USER_ID).await });
        let submitted = server.request().await;
        assert_eq!(
            form_fields(&submitted.body).get("refresh_token"),
            Some(&"refresh-once".to_owned())
        );

        session.shutdown().await;
        assert!(matches!(request.await, Ok(Err(AuthError::SessionClosed))));
        let replacement = {
            let authority = oauth_authority(server.origin.clone(), Arc::clone(&transport));
            let (sender, _) = watch::channel(SessionChange::default());
            AuthSession::spawn_with_control(
                store,
                Arc::new(SessionControl::new(0, 0, sender)),
                authority,
            )
            .unwrap()
        };
        assert!(matches!(
            replacement.profile(TEST_USER_ID).await,
            Err(AuthError::RefreshOutcomeUnknown)
        ));
        assert!(server.requests.try_recv().is_err());

        replacement.shutdown().await;
        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn ambiguous_refresh_rotation_is_never_replayed() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, changes, _) =
            test_session_with_authority_and_control(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "initial-access",
                    RENEWAL_REFRESH_EARLY_SECS,
                    PlanCode::Free,
                ),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .unwrap();
        let profile = session.profile(TEST_USER_ID).await.unwrap();
        let committed_change = *changes.borrow();
        authority.fail_next(AuthError::RefreshOutcomeUnknown);

        assert!(matches!(
            session
                .renewal_headers(
                    profile.clone(),
                    "GET",
                    "https://api.kytrix.xyz/",
                    "first-attempt",
                )
                .await,
            Err(AuthError::RefreshOutcomeUnknown)
        ));
        assert!(matches!(
            session
                .renewal_headers(profile, "GET", "https://api.kytrix.xyz/", "second-attempt",)
                .await,
            Err(AuthError::RefreshOutcomeUnknown)
        ));

        assert_eq!(authority.calls().len(), 1);
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);
        assert_ne!(*changes.borrow(), committed_change);
        assert_eq!(
            changes.borrow().availability(),
            SessionAvailability::Unavailable
        );
        let credential_store: Arc<dyn CredentialStore> = store;
        assert!(matches!(
            load_credential(&credential_store),
            Ok(LoadedCredential::RefreshOutcomeUnknown)
        ));
    }

    #[tokio::test]
    async fn noncanonical_persisted_user_id_tombstones_before_authority_across_owners() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
            "refresh-1",
            &key,
        ))));
        let authority_a = Arc::new(FakeRefreshAuthority::new());
        let (session_a, mut changes_a) =
            test_session_with_authority(store.clone(), 0, 0, authority_a.clone());

        assert!(matches!(
            session_a.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        changes_a.changed().await.unwrap();
        assert_eq!(
            changes_a.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(authority_a.calls().is_empty());
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the corrupt record should become a tombstone")
            ),
            Ok(StoredCredential::Invalidated)
        ));

        drop(session_a);
        let authority_b = Arc::new(FakeRefreshAuthority::new());
        let (session_b, changes_b) = test_session_with_authority(store, 0, 0, authority_b.clone());
        assert!(matches!(
            session_b.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority_b.calls().is_empty());
        assert!(!changes_b.has_changed().unwrap());
    }

    #[tokio::test]
    async fn local_status_reports_persisted_tombstone_without_identity_or_event() {
        let payload = serde_json::to_vec(&StoredCredential::Invalidated).unwrap();
        let store = Arc::new(StallingCredentialStore::new(Some(payload.into())));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (changes, receiver) = watch::channel(SessionChange::default());
        let control = Arc::new(SessionControl::new(0, 0, changes));
        let session =
            AuthSession::spawn_with_control(store, control.clone(), authority.clone()).unwrap();

        let status = session.local_status().await.unwrap();

        assert_eq!(status.availability(), SessionAvailability::Unavailable);
        assert_eq!(status.observed_change(), SessionChange::default());
        assert_eq!(control.revision.load(Ordering::Acquire), 0);
        assert_eq!(control.epoch.load(Ordering::Acquire), 0);
        assert!(authority.calls().is_empty());
        assert!(!receiver.has_changed().unwrap());
    }

    #[tokio::test]
    async fn local_status_keeps_valid_unverified_or_missing_records_unknown() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let active_store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let active_authority = Arc::new(FakeRefreshAuthority::new());
        let (active, active_changes) =
            test_session_with_authority(active_store, 0, 0, active_authority.clone());
        let missing_authority = Arc::new(FakeRefreshAuthority::new());
        let (missing, missing_changes) = test_session_with_authority(
            Arc::new(StallingCredentialStore::new(None)),
            0,
            0,
            missing_authority.clone(),
        );

        assert_eq!(
            active.local_status().await.unwrap().availability(),
            SessionAvailability::Unknown
        );
        assert_eq!(
            missing.local_status().await.unwrap().availability(),
            SessionAvailability::Unknown
        );
        assert!(active_authority.calls().is_empty());
        assert!(missing_authority.calls().is_empty());
        assert!(!active_changes.has_changed().unwrap());
        assert!(!missing_changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn local_status_tombstones_a_corrupt_record_without_authority_or_event() {
        let store = Arc::new(StallingCredentialStore::new(Some(Bytes::from_static(
            b"not-json",
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());

        let status = session.local_status().await.unwrap();

        assert_eq!(status.availability(), SessionAvailability::Unavailable);
        assert!(authority.calls().is_empty());
        assert!(!changes.has_changed().unwrap());
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the corrupt record should become a tombstone")
            ),
            Ok(StoredCredential::Invalidated)
        ));
    }

    #[test]
    fn session_revision_reaches_max_then_reports_exhaustion_without_reuse() {
        let counter = AtomicU64::new(u64::MAX - 1);

        assert_eq!(
            next_monotonic(&counter).expect("the maximum revision should be allocated once"),
            u64::MAX
        );
        assert!(matches!(
            next_monotonic(&counter),
            Err(AuthError::SequenceExhausted)
        ));
        assert!(matches!(
            next_monotonic(&counter),
            Err(AuthError::SequenceExhausted)
        ));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    /// A refresh the authority refuses is what retires the record, and the tombstone it
    /// leaves is what every owner after it reads: none of them asks the authority again,
    /// and only a login that completes replaces it.
    #[tokio::test]
    async fn invalid_grant_tombstone_survives_owner_rebuild_and_failed_install() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority_a = Arc::new(FakeRefreshAuthority::new());
        authority_a.fail_next(AuthError::InvalidGrant);
        let (session_a, mut changes_a) =
            test_session_with_authority(store.clone(), 0, 0, authority_a.clone());

        assert!(matches!(
            session_a.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidGrant)
        ));
        changes_a.changed().await.unwrap();
        assert_eq!(
            changes_a.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session_a.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert_eq!(
            authority_a.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the tombstone should persist")
            ),
            Ok(StoredCredential::Invalidated)
        ));
        drop(session_a);

        // The next owner reads the tombstone and refuses the record without asking the
        // authority anything, whatever the caller asks for.
        let authority_b = Arc::new(FakeRefreshAuthority::new());
        let (session_b, changes_b) =
            test_session_with_authority(store.clone(), 0, 0, authority_b.clone());
        assert!(matches!(
            session_b.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            session_b
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority_b.calls().is_empty());
        assert!(!changes_b.has_changed().unwrap());

        // A login that cannot be persisted leaves the tombstone in place: the session it
        // would carry never happened, so nothing may serve it.
        store.fail_next_store();
        assert!(matches!(
            session_b
                .install(
                    token_response(TEST_REPLACEMENT_USER_ID, "refresh-2", "access-2"),
                    DpopKey::from_signing_key(SigningKey::generate()),
                )
                .await,
            Err(AuthError::CredentialStore)
        ));
        assert!(matches!(
            session_b.profile(TEST_REPLACEMENT_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority_b.calls().is_empty());
        assert!(!changes_b.has_changed().unwrap());
        drop(session_b);

        let authority_c = Arc::new(FakeRefreshAuthority::new());
        let (session_c, mut changes_c) =
            test_session_with_authority(store.clone(), 0, 0, authority_c.clone());
        assert!(matches!(
            session_c.profile(TEST_REPLACEMENT_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority_c.calls().is_empty());
        assert!(!changes_c.has_changed().unwrap());

        session_c
            .install(
                token_response(TEST_REPLACEMENT_USER_ID, "refresh-3", "access-3"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("a persisted explicit install should recover the session");
        changes_c.changed().await.unwrap();
        assert_eq!(
            changes_c.borrow_and_update().availability(),
            SessionAvailability::Available
        );
        let replacement = session_c
            .profile(TEST_REPLACEMENT_USER_ID)
            .await
            .expect("the replacement profile should be active");
        assert_ne!(replacement.credential_revision(), 0);
        assert_ne!(replacement.credential_epoch(), 0);
        drop(session_c);

        let authority_d = Arc::new(FakeRefreshAuthority::new());
        let (session_d, mut changes_d) =
            test_session_with_authority(store.clone(), 0, 0, authority_d.clone());
        session_d
            .profile(TEST_REPLACEMENT_USER_ID)
            .await
            .expect("a new owner should validate the persisted replacement");
        changes_d.changed().await.unwrap();
        assert_eq!(
            changes_d.borrow_and_update().availability(),
            SessionAvailability::Available
        );
        assert_eq!(
            authority_d.calls(),
            vec![(TEST_REPLACEMENT_USER_ID.to_owned(), "refresh-3".to_owned())]
        );
    }

    #[tokio::test]
    async fn malformed_stored_credential_tombstone_survives_owner_rebuild() {
        let store = Arc::new(StallingCredentialStore::new(Some(Bytes::from_static(
            b"not-json",
        ))));
        let (session_a, mut changes_a) = test_session(store.clone(), 0, 0);

        assert!(matches!(
            session_a.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        changes_a.changed().await.unwrap();
        assert_eq!(
            changes_a.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        drop(session_a);

        let authority_b = Arc::new(FakeRefreshAuthority::new());
        let (session_b, changes_b) =
            test_session_with_authority(store.clone(), 0, 0, authority_b.clone());
        assert!(matches!(
            session_b.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            session_b
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority_b.calls().is_empty());
        assert!(!changes_b.has_changed().unwrap());
    }

    /// The tombstone is what records that a rejected refresh retired the session. A store
    /// that refuses it cannot make the session that could not be retired usable again: the
    /// owner gives up the credential rather than serving one it can no longer vouch for.
    #[tokio::test]
    async fn a_rejected_refresh_that_cannot_be_recorded_is_still_refused_a_credential() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        authority.fail_next(AuthError::InvalidGrant);
        // The refresh writes its replay barrier before the authority is asked, and that
        // write is the one thing a rejection does not replace: it is the tombstone the
        // store refuses here.
        store.fail_next_store();
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());

        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::CredentialStore)
        ));
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            session
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );
        assert_eq!(store.stores.load(Ordering::Acquire), 2);
        drop(session);

        // What the store kept is the barrier, so the next start gives up on the record by
        // itself instead of replaying a token the authority has already refused.
        let replacement_authority = Arc::new(FakeRefreshAuthority::new());
        let (replacement, mut replacement_changes) =
            test_session_with_authority(store.clone(), 0, 0, replacement_authority.clone());
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
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the barrier should be what the store kept")
            ),
            Ok(StoredCredential::RefreshOutcomeUnknown)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cold_invalid_grant_publishes_unavailable_before_tombstone_store_finishes() {
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
            test_session_with_authority(store.clone(), 0, 0, authority.clone());
        let session = Arc::new(session);
        let request_session = session.clone();
        let request = tokio::spawn(async move { request_session.profile(TEST_USER_ID).await });

        store.started.notified().await;
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(!request.is_finished());
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the refresh replay barrier should remain persisted")
            ),
            Ok(StoredCredential::RefreshOutcomeUnknown)
        ));

        store.release();
        assert!(matches!(
            request.await.expect("the auth command should finish"),
            Err(AuthError::InvalidGrant)
        ));
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);
        assert!(!changes.has_changed().unwrap());
        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_invalid_grant_publishes_unavailable_before_tombstone_store_finishes() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "access-1",
                    0,
                    PlanCode::Free,
                ),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("the active session should install");
        changes.changed().await.unwrap();
        changes.borrow_and_update();
        let profile = session.profile(TEST_USER_ID).await.unwrap();
        authority.fail_next(AuthError::InvalidGrant);
        store.stall_next_store();
        let session = Arc::new(session);
        let request_session = session.clone();
        let request = tokio::spawn(async move {
            request_session
                .headers(Some(profile), "GET", "https://api.kytrix.xyz/")
                .await
        });

        store.started.notified().await;
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(!request.is_finished());

        store.release();
        assert!(matches!(
            request.await.expect("the auth command should finish"),
            Err(AuthError::InvalidGrant)
        ));
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(!changes.has_changed().unwrap());
        assert_eq!(authority.calls().len(), 1);
    }

    /// A stale access token costs a rotation, and a rotation writes its replay barrier
    /// before it asks the authority: until that write lands there is nothing submitted and
    /// nothing published, and once it does the command commits what it was waiting for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expired_rotation_persists_barrier_before_authority_submission() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "access-1",
                    0,
                    PlanCode::Free,
                ),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("the active session should install");
        changes.changed().await.unwrap();
        changes.borrow_and_update();
        store.stall_next();
        let session = Arc::new(session);
        let request_session = session.clone();
        let request = tokio::spawn(async move {
            request_session
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await
        });

        store.started.notified().await;
        assert!(!request.is_finished());
        assert!(authority.calls().is_empty());
        assert!(!changes.has_changed().unwrap());

        store.release();
        let (profile, (access_token, _)) = request
            .await
            .expect("the headers command should finish")
            .expect("the rotation should commit");
        assert_eq!(profile.account_id(), TEST_USER_ID);
        assert_eq!(access_token, "access-1");
        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );
        assert_eq!(stored_active_credential(&store).1, "refresh-1");
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn a_sign_out_removes_the_credential_and_ends_the_session_in_one_step() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response(TEST_USER_ID, "refresh-1", "access-1"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("the active session should install");
        changes.changed().await.unwrap();
        changes.borrow_and_update();

        let discarded = session.discard_local_session();

        assert!(discarded.removed());
        assert!(store.load().is_err());
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        // Nothing has been said to the authority yet: the report is the caller's to send.
        assert!(authority.revocations().is_empty());

        let revocation = discarded
            .into_revocation()
            .expect("the signed-out session should hand over its token");
        session.revoke_session(revocation).await;
        assert_eq!(authority.revocations(), vec!["refresh-1".to_owned()]);
    }

    #[tokio::test]
    async fn a_sign_out_is_unaffected_by_a_revocation_the_authority_refuses() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response(TEST_USER_ID, "refresh-1", "access-1"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("the active session should install");
        changes.changed().await.unwrap();
        changes.borrow_and_update();
        authority.fail_next_revoke(AuthError::Request);

        let discarded = session.discard_local_session();
        session
            .revoke_session(
                discarded
                    .into_revocation()
                    .expect("the signed-out session should hand over its token"),
            )
            .await;

        assert_eq!(authority.revocations(), vec!["refresh-1".to_owned()]);
        assert!(store.load().is_err());
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
    }

    /// A store that refuses both the removal and the record that replaces it is the one
    /// case a sign-out cannot finish: what is left there still grants access, and the
    /// report says so instead of calling the machine clean.
    #[tokio::test]
    async fn a_sign_out_a_store_refuses_reports_the_credential_it_left_behind() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response(TEST_USER_ID, "refresh-1", "access-1"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("the active session should install");
        changes.changed().await.unwrap();
        changes.borrow_and_update();
        store.fail_next_delete();
        store.fail_next_store();

        let discarded = session.discard_local_session();

        assert!(
            !discarded.removed(),
            "a store still holding a usable record is not one a sign-out may call clean"
        );
        assert_eq!(store.deletes.load(Ordering::Acquire), 1);
        assert_eq!(stored_active_credential(&store).1, "refresh-1");
        // The session is over here whatever the store kept of it: what survived is not
        // something this process will serve again.
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            discarded.into_revocation(),
            Some(revocation) if revocation.refresh_token() == "refresh-1"
        ));
    }

    /// A sign-out can land while a rotation is already inside the store, and the record
    /// then arrives after the removal. It belongs to the session the user signed out of —
    /// the token in it is the rotated one, which is what the next start would sign back in
    /// with — so the operation that wrote it takes it away again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rotation_that_lands_after_a_sign_out_is_taken_back() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, _) = test_session_with_authority(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "access-1",
                    0,
                    PlanCode::Free,
                ),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("the active session should install");
        // The rotation commit is the write that stalls: the barrier is written and the
        // authority has answered by then, so the store is about to take a rotated token.
        store.stall_next_store();
        let session = Arc::new(session);
        let request_session = session.clone();
        let request = tokio::spawn(async move {
            request_session
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await
        });
        store.started.notified().await;

        let discarded = session.discard_local_session();
        assert!(discarded.removed());
        store.release();

        assert!(matches!(
            request.await.expect("the headers command should finish"),
            Err(AuthError::SessionClosed)
        ));
        assert_eq!(
            store.deletes.load(Ordering::Acquire),
            2,
            "the sign-out removed the record, and the write that arrived late was taken back"
        );
        let credential_store: Arc<dyn CredentialStore> = store;
        assert!(
            matches!(
                load_credential(&credential_store),
                Ok(LoadedCredential::Invalidated)
            ),
            "what is left of the machine grants nothing"
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
    }

    #[tokio::test]
    async fn revoke_response_loss_should_leave_a_signed_out_session_signed_out() {
        let mut server = TestOAuthServer::spawn(vec![OAuthHttpBehavior::DropResponse]).await;
        let transport = test_transport();
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = oauth_authority(server.origin.clone(), Arc::clone(&transport));
        let (sender, _) = watch::channel(SessionChange::default());
        let session = AuthSession::spawn_with_control(
            store.clone(),
            Arc::new(SessionControl::new(0, 0, sender)),
            authority,
        )
        .expect("test auth session should start");
        let mut changes = session.session_changes();
        session
            .install(
                token_response(TEST_USER_ID, "refresh-1", "access-1"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("active credential should install");
        changes.changed().await.unwrap();
        changes.borrow_and_update();

        let discarded = session.discard_local_session();
        session
            .revoke_session(
                discarded
                    .into_revocation()
                    .expect("the signed-out session should hand over its token"),
            )
            .await;
        let request = server.request().await;

        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/oauth/revoke");
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        // The record was removed rather than tombstoned: there is nothing left for a
        // later start to turn back into a session.
        assert!(store.load().is_err());
        assert!(server.requests.try_recv().is_err());

        session.shutdown().await;
        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cold_rotation_failure_publishes_before_stalled_tombstone_store() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        authority.rotate_next("refresh-2");
        store.fail_next_store();
        store.stall_store_after(2);
        let (session, mut changes) = test_session_with_authority(store.clone(), 0, 0, authority);
        let session = Arc::new(session);
        let request_session = session.clone();
        let request = tokio::spawn(async move { request_session.profile(TEST_USER_ID).await });

        store.started.notified().await;
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(!request.is_finished());
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the refresh replay barrier should remain persisted")
            ),
            Ok(StoredCredential::RefreshOutcomeUnknown)
        ));

        store.release();
        assert!(matches!(
            request.await.expect("the auth command should finish"),
            Err(AuthError::CredentialStore)
        ));
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn cold_network_failure_keeps_active_record_unpublished_and_retryable() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        authority.fail_next(AuthError::Request);
        authority.rotate_next("refresh-2");
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());

        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::Request)
        ));
        assert_eq!(stored_active_credential(&store).1, "refresh-1");
        assert_eq!(store.stores.load(Ordering::Acquire), 2);
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        assert!(!changes.has_changed().unwrap());
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);

        let profile = session
            .profile(TEST_USER_ID)
            .await
            .expect("a transient network failure should remain retryable");
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Available
        );
        assert_ne!(profile.credential_revision(), 0);
        assert_eq!(stored_active_credential(&store).1, "refresh-2");
        assert_eq!(
            authority.calls(),
            vec![
                (TEST_USER_ID.to_owned(), "refresh-1".to_owned()),
                (TEST_USER_ID.to_owned(), "refresh-1".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn cold_rotation_store_failure_tombstones_and_requires_explicit_install() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        authority.rotate_next("refresh-2");
        store.fail_next_store();
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());

        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::CredentialStore)
        ));
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert_eq!(session.current_plan(TEST_USER_ID).await.unwrap(), None);
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            session
                .headers(None, "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the failed rotation should persist a tombstone")
            ),
            Ok(StoredCredential::Invalidated)
        ));
        assert!(!changes.has_changed().unwrap());
        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );

        drop(session);
        let replacement_authority = Arc::new(FakeRefreshAuthority::new());
        let (replacement, mut replacement_changes) =
            test_session_with_authority(store.clone(), 0, 0, replacement_authority.clone());
        assert!(matches!(
            replacement.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(replacement_authority.calls().is_empty());
        assert!(!replacement_changes.has_changed().unwrap());

        replacement
            .install(
                token_response(TEST_REPLACEMENT_USER_ID, "refresh-3", "access-3"),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("an explicit login should replace the tombstone");
        replacement_changes.changed().await.unwrap();
        assert_eq!(
            replacement_changes.borrow_and_update().availability(),
            SessionAvailability::Available
        );
        replacement
            .profile(TEST_REPLACEMENT_USER_ID)
            .await
            .expect("the explicit login should recover the session");
    }

    #[tokio::test]
    async fn active_rotation_store_failure_tombstones_without_replaying() {
        let store = Arc::new(StallingCredentialStore::new(None));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());
        session
            .install(
                token_response_with_expiry(
                    TEST_USER_ID,
                    "refresh-1",
                    "access-1",
                    0,
                    PlanCode::Free,
                ),
                DpopKey::from_signing_key(SigningKey::generate()),
            )
            .await
            .expect("the active session should install");
        changes.changed().await.unwrap();
        changes.borrow_and_update();
        let profile = session.profile(TEST_USER_ID).await.unwrap();
        authority.rotate_next("refresh-2");
        store.fail_next_store();

        assert!(matches!(
            session
                .headers(Some(profile.clone()), "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::CredentialStore)
        ));
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            session
                .headers(Some(profile), "GET", "https://api.kytrix.xyz/")
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );

        drop(session);
        let next_authority = Arc::new(FakeRefreshAuthority::new());
        let (next, next_changes) = test_session_with_authority(store, 0, 0, next_authority.clone());
        assert!(matches!(
            next.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(next_authority.calls().is_empty());
        assert!(!next_changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn failed_rotation_tombstone_is_still_fail_closed_for_the_current_owner() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        store.fail_next_stores(2);
        let authority = Arc::new(FakeRefreshAuthority::new());
        authority.rotate_next("refresh-2");
        let (session, mut changes) =
            test_session_with_authority(store.clone(), 0, 0, authority.clone());

        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::CredentialStore)
        ));
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the failed rotation must retain its replay barrier")
            ),
            Ok(StoredCredential::RefreshOutcomeUnknown)
        ));
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert_eq!(authority.calls().len(), 1);

        drop(session);
        let cold_authority = Arc::new(FakeRefreshAuthority::new());
        let (cold, mut cold_changes) =
            test_session_with_authority(store.clone(), 0, 0, cold_authority.clone());
        assert!(matches!(
            cold.profile(TEST_USER_ID).await,
            Err(AuthError::RefreshOutcomeUnknown)
        ));
        cold_changes.changed().await.unwrap();
        assert_eq!(
            cold_changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(cold_authority.calls().is_empty());
        assert!(matches!(
            serde_json::from_slice::<StoredCredential>(
                store
                    .payload
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    .expect("the cold validation should persist a tombstone")
            ),
            Ok(StoredCredential::RefreshOutcomeUnknown)
        ));
    }

    #[tokio::test]
    async fn caller_account_mismatch_does_not_tombstone_a_valid_session() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let (session, mut changes) = test_session(store.clone(), 0, 0);
        let profile = session
            .profile(TEST_USER_ID)
            .await
            .expect("the persisted credential should validate");
        changes.changed().await.unwrap();
        changes.borrow_and_update();

        assert!(matches!(
            session.profile(TEST_OTHER_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            session
                .headers(
                    Some(AccountProfile::new(
                        TEST_OTHER_USER_ID,
                        profile.credential_revision(),
                        profile.credential_epoch(),
                    )),
                    "GET",
                    "https://api.kytrix.xyz/",
                )
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert_eq!(
            session
                .profile(TEST_USER_ID)
                .await
                .expect("the valid session should remain active"),
            profile
        );
        assert_eq!(stored_active_credential(&store).1, "refresh-1");
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn cold_profile_mismatch_rejects_before_authority_or_identity_allocation() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let payload = credential_payload(TEST_USER_ID, "refresh-1", &key);
        let store = Arc::new(StallingCredentialStore::new(Some(payload.clone())));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes, control) =
            test_session_with_authority_and_control(store.clone(), 0, 0, authority.clone());

        assert!(matches!(
            session.profile(TEST_OTHER_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority.calls().is_empty());
        assert_eq!(store.stores.load(Ordering::Acquire), 0);
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        assert_eq!(control.revision.load(Ordering::Acquire), 0);
        assert_eq!(control.epoch.load(Ordering::Acquire), 0);
        assert_eq!(
            store
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_deref(),
            Some(payload.as_ref())
        );
        assert!(!changes.has_changed().unwrap());
        drop(session);

        let recovered = AuthSession::spawn_with_control(store, control, authority.clone())
            .expect("the replacement auth actor should start");
        let profile = recovered
            .profile(TEST_USER_ID)
            .await
            .expect("the matching account should validate the untouched record");
        changes.changed().await.unwrap();
        assert_eq!(profile.credential_revision(), 1);
        assert_eq!(profile.credential_epoch(), 1);
        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );
    }

    #[tokio::test]
    async fn cold_headers_mismatch_rejects_before_authority_or_identity_allocation() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let payload = credential_payload(TEST_USER_ID, "refresh-1", &key);
        let store = Arc::new(StallingCredentialStore::new(Some(payload.clone())));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes, control) =
            test_session_with_authority_and_control(store.clone(), 0, 0, authority.clone());

        assert!(matches!(
            session
                .headers(
                    Some(AccountProfile::new(TEST_OTHER_USER_ID, 7, 9)),
                    "GET",
                    "https://api.kytrix.xyz/",
                )
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority.calls().is_empty());
        assert_eq!(store.stores.load(Ordering::Acquire), 0);
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        assert_eq!(control.revision.load(Ordering::Acquire), 0);
        assert_eq!(control.epoch.load(Ordering::Acquire), 0);
        assert_eq!(
            store
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_deref(),
            Some(payload.as_ref())
        );
        assert!(!changes.has_changed().unwrap());
        drop(session);

        let recovered = AuthSession::spawn_with_control(store, control, authority.clone())
            .expect("the replacement auth actor should start");
        let (profile, _) = recovered
            .headers(None, "GET", "https://api.kytrix.xyz/")
            .await
            .expect("the matching account should validate the untouched record");
        changes.changed().await.unwrap();
        assert_eq!(profile.credential_revision(), 1);
        assert_eq!(profile.credential_epoch(), 1);
        assert_eq!(
            authority.calls(),
            vec![(TEST_USER_ID.to_owned(), "refresh-1".to_owned())]
        );
    }

    /// A sign-out that lands while a refresh is waiting for the authority is the case the
    /// credential store is most at risk from: that refresh loaded its own copy of the
    /// token and would write the rotated one back the moment it came home.
    ///
    /// The store it writes through is the real one, so the sign-out is taken once the
    /// refresh has stopped touching it and is waiting on the authority: what the two can
    /// still contend over is the record, which is exactly what this asserts about.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sign_out_during_a_refresh_leaves_no_credential_to_write_back() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority = Arc::new(StallingRefreshAuthority::new());
        let (changes, receiver) = watch::channel(SessionChange::default());
        let session = AuthSession::spawn_with_control(
            store.clone(),
            Arc::new(SessionControl::new(0, 0, changes)),
            authority.clone(),
        )
        .expect("the test auth actor should start");
        let mut changes = receiver;

        let refreshing = session.clone();
        let refresh = tokio::spawn(async move { refreshing.profile(TEST_USER_ID).await });
        authority.started.notified().await;

        let discarded = session.discard_local_session();

        assert!(
            discarded.removed(),
            "the credential is gone from this machine"
        );
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );

        // The refresh was holding a token that no longer belongs to anything: the command
        // it was serving is refused instead of waiting for it, and the request it had out
        // goes with it rather than holding the session until an unusable answer arrives.
        assert!(matches!(
            refresh
                .await
                .expect("the interrupted command should be answered"),
            Err(AuthError::SessionClosed)
        ));
        assert!(authority.future_dropped.load(Ordering::Acquire));
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(
            store
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "a signed-out machine keeps no credential to sign back in with"
        );
        assert_eq!(
            store.stores.load(Ordering::Acquire),
            1,
            "only the refresh barrier was ever written"
        );
        assert_eq!(
            session.current_plan(TEST_USER_ID).await.unwrap(),
            None,
            "a signed-out session has no plan to report"
        );
    }

    #[tokio::test]
    async fn a_sign_out_before_the_session_is_loaded_ends_it_without_a_command() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-1",
            &key,
        ))));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, mut changes, control) =
            test_session_with_authority_and_control(store.clone(), 0, 0, authority.clone());
        assert_eq!(store.loads.load(Ordering::Acquire), 0);

        let discarded = session.discard_local_session();

        assert!(discarded.removed());
        // A session that was never loaded still knows which token it holds: the credential
        // is where the token is, and the sign-out reads it rather than asking the actor.
        let revocation = discarded
            .into_revocation()
            .expect("the stored credential still held a token");
        assert_eq!(revocation.refresh_token(), "refresh-1");
        assert!(store.load().is_err());
        assert_eq!(
            changes.borrow_and_update().availability(),
            SessionAvailability::Unavailable
        );
        assert!(matches!(
            session.profile(TEST_USER_ID).await,
            Err(AuthError::InvalidCredential)
        ));
        // Nothing was asked of the authority, and no identity was allocated for a
        // session that never existed: the only change is the credential's removal and
        // the change that says the session is unavailable.
        assert!(authority.calls().is_empty());
        assert_eq!(control.revision.load(Ordering::Acquire), 0);
        assert_eq!(control.epoch.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn cold_noncanonical_expected_ids_leave_a_valid_record_untouched() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let payload = credential_payload(TEST_USER_ID, "refresh-1", &key);
        let store = Arc::new(StallingCredentialStore::new(Some(payload.clone())));
        let authority = Arc::new(FakeRefreshAuthority::new());
        let (session, changes, control) =
            test_session_with_authority_and_control(store.clone(), 0, 0, authority.clone());

        assert!(matches!(
            session
                .profile("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA")
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(matches!(
            session
                .headers(
                    Some(AccountProfile::new("not-a-uuid", 1, 1)),
                    "GET",
                    "https://api.kytrix.xyz/",
                )
                .await,
            Err(AuthError::InvalidCredential)
        ));
        assert!(authority.calls().is_empty());
        assert!(authority.revocations().is_empty());
        assert_eq!(store.stores.load(Ordering::Acquire), 0);
        assert_eq!(store.deletes.load(Ordering::Acquire), 0);
        assert_eq!(control.revision.load(Ordering::Acquire), 0);
        assert_eq!(control.epoch.load(Ordering::Acquire), 0);
        assert_eq!(
            store
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_deref(),
            Some(payload.as_ref())
        );
        assert!(!changes.has_changed().unwrap());
    }

    #[test]
    fn session_changes_advance_even_when_availability_is_unchanged() {
        let counter = AtomicU64::new(0);
        let first = next_session_change_with(&counter, SessionAvailability::Available).unwrap();
        let second = next_session_change_with(&counter, SessionAvailability::Available).unwrap();

        assert_eq!(first.epoch(), Some(1));
        assert_eq!(second.epoch(), Some(2));
        assert_eq!(second.availability(), SessionAvailability::Available);
    }

    #[test]
    fn session_change_epoch_reaches_max_then_reports_exhaustion_without_reuse() {
        let counter = AtomicU64::new(u64::MAX - 1);

        let maximum = next_session_change_with(&counter, SessionAvailability::Available)
            .expect("the maximum epoch should be allocated once");
        assert_eq!(maximum.epoch(), Some(u64::MAX));
        assert!(matches!(
            next_session_change_with(&counter, SessionAvailability::Unavailable),
            Err(AuthError::SequenceExhausted)
        ));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[tokio::test]
    async fn sequence_exhaustion_publishes_one_terminal_invalidation() {
        let (sender, mut changes) = watch::channel(SessionChange::published(
            u64::MAX,
            SessionAvailability::Available,
        ));
        let initial = *changes.borrow_and_update();

        publish_session_exhaustion_to(&sender);
        changes.changed().await.unwrap();
        let exhausted = *changes.borrow_and_update();
        publish_session_exhaustion_to(&sender);

        assert_eq!(initial.epoch(), Some(u64::MAX));
        assert!(exhausted.is_exhausted());
        assert_eq!(exhausted.availability(), SessionAvailability::Unavailable);
        assert!(!changes.has_changed().unwrap());
    }
}

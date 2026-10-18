use std::sync::Arc;

use bytes::Bytes;

use super::{
    AuthError, CREDENTIAL_SERVICE, CREDENTIAL_USER, DpopKey, LoadedCredential,
    MAX_CREDENTIAL_BYTES, SessionControl, StoredCredential, canonical_user_id,
};

pub(super) trait CredentialStore: Send + Sync + 'static {
    fn load(&self) -> Result<Bytes, AuthError>;
    fn store(&self, payload: Bytes) -> Result<(), AuthError>;
    fn delete(&self) -> Result<(), AuthError>;
}

pub(super) struct KeyringCredentialStore;

impl CredentialStore for KeyringCredentialStore {
    fn load(&self) -> Result<Bytes, AuthError> {
        let entry = keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_USER)
            .map_err(|_| AuthError::CredentialStore)?;
        entry
            .get_secret()
            .map(Bytes::from)
            .map_err(|error| match error {
                keyring::Error::NoEntry => AuthError::MissingCredential,
                _ => AuthError::CredentialStore,
            })
    }

    fn store(&self, payload: Bytes) -> Result<(), AuthError> {
        keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_USER)
            .and_then(|entry| entry.set_secret(payload.as_ref()))
            .map_err(|_| AuthError::CredentialStore)
    }

    fn delete(&self) -> Result<(), AuthError> {
        let entry = keyring::Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_USER)
            .map_err(|_| AuthError::CredentialStore)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(AuthError::CredentialStore),
        }
    }
}

pub(super) fn load_credential(
    store: &std::sync::Arc<dyn CredentialStore>,
) -> Result<LoadedCredential, AuthError> {
    let credential = store.load()?;
    if credential.len() > MAX_CREDENTIAL_BYTES {
        return Err(AuthError::InvalidCredential);
    }
    let credential: StoredCredential =
        serde_json::from_slice(&credential).map_err(|_| AuthError::InvalidCredential)?;
    match credential {
        StoredCredential::Active {
            user_id,
            refresh_token,
            private_key,
        } => {
            if !canonical_user_id(&user_id) || refresh_token.is_empty() {
                return Err(AuthError::InvalidCredential);
            }
            let key = DpopKey::decode_private(&private_key)?;
            Ok(LoadedCredential::Active {
                user_id,
                refresh_token,
                key,
            })
        }
        StoredCredential::RefreshOutcomeUnknown => Ok(LoadedCredential::RefreshOutcomeUnknown),
        StoredCredential::Invalidated => Ok(LoadedCredential::Invalidated),
    }
}

/// Writes the credential that proves a session.
///
/// Every write goes through the control's permission first: once the local credential
/// has been discarded by a sign-out, no operation that was already under way may put one
/// back, because the record it would restore is exactly the one the user just removed.
pub(super) fn store_credential(
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
    user_id: &str,
    refresh_token: &str,
    key: &DpopKey,
) -> Result<(), AuthError> {
    control.credential_is_current()?;
    let payload = serde_json::to_vec(&StoredCredential::Active {
        user_id: user_id.to_owned(),
        refresh_token: refresh_token.to_owned(),
        private_key: key.encode_private(),
    })
    .map(Bytes::from)
    .map_err(|_| AuthError::InvalidResponse)?;
    if payload.len() > MAX_CREDENTIAL_BYTES {
        return Err(AuthError::InvalidResponse);
    }
    store.store(payload)?;
    confirm_stored_credential_is_current(store, control)
}

/// Confirms that the record just written outlived no sign-out.
///
/// A sign-out runs on the caller while a write is already in the store, so the write can
/// land after the removal. What arrived late is the record of the session the user signed
/// out of — a rotated token is exactly what would sign them back in on the next start — so
/// the operation that wrote it is the one that takes it away again. Only this operation
/// can be writing here: the actor serves one command at a time.
fn confirm_stored_credential_is_current(
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
) -> Result<(), AuthError> {
    match control.credential_is_current() {
        Ok(()) => Ok(()),
        Err(error) => {
            retire_stored_credential(store);
            Err(error)
        }
    }
}

/// Replaces the credential with one that grants nothing.
///
/// A credential store can refuse a deletion. What it is left holding afterwards must then
/// be a record no later start can turn back into a session, which is what this writes.
pub(super) fn store_invalidated_credential(
    store: &Arc<dyn CredentialStore>,
) -> Result<(), AuthError> {
    let payload = serde_json::to_vec(&StoredCredential::Invalidated)
        .map(Bytes::from)
        .map_err(|_| AuthError::InvalidResponse)?;
    store.store(payload)
}

/// Makes sure the store holds nothing that can sign this machine in.
///
/// The removal is what a sign-out is for. A store that refuses it keeps the record it had,
/// which is the one that can sign the user back in, so it is left holding a record that
/// grants nothing instead. Only a store that refuses both leaves something usable behind,
/// which is what the returned flag reports.
pub(super) fn retire_stored_credential(store: &Arc<dyn CredentialStore>) -> bool {
    match store.delete() {
        Ok(()) => true,
        Err(error) => {
            log::warn!("failed to remove the stored account credential: {error}");
            match store_invalidated_credential(store) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("failed to retire the stored account credential: {error}");
                    false
                }
            }
        }
    }
}

pub(super) fn store_refresh_outcome_unknown(
    store: &Arc<dyn CredentialStore>,
    control: &SessionControl,
) -> Result<(), AuthError> {
    control.credential_is_current()?;
    let payload = serde_json::to_vec(&StoredCredential::RefreshOutcomeUnknown)
        .map(Bytes::from)
        .map_err(|_| AuthError::InvalidResponse)?;
    store.store(payload)?;
    confirm_stored_credential_is_current(store, control)
}

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

pub(super) fn store_invalidated_credential(
    store: &Arc<dyn CredentialStore>,
) -> Result<(), AuthError> {
    let payload = serde_json::to_vec(&StoredCredential::Invalidated)
        .map(Bytes::from)
        .map_err(|_| AuthError::InvalidResponse)?;
    store.store(payload)
}

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

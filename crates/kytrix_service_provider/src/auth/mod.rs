pub mod constants;
mod credential;
pub mod dpop;
pub mod model;
mod oauth;
pub mod session;
mod state;
#[cfg(test)]
pub(crate) mod test_support;

use constants::{
    ACCOUNT_ORIGIN, API_RESOURCE, AUTH_COMMAND_CAPACITY, CLIENT_ID, CREDENTIAL_SERVICE,
    CREDENTIAL_USER, MAX_CREDENTIAL_BYTES, MAX_RESPONSE_BYTES, REFRESH_EARLY_SECS,
    RENEWAL_REFRESH_EARLY_SECS, SCOPE, TOKEN_TTL_SECS,
};
use credential::{
    CredentialStore, KeyringCredentialStore, load_credential, retire_stored_credential,
    store_credential, store_invalidated_credential, store_refresh_outcome_unknown,
};
use dpop::DpopKey;
use model::{
    AccessToken, AccountIdentity, AccountWalletKind, AuthAuthority, AuthCommand, AuthError,
    AuthorizationProofContext, LoadedCredential, LocalSessionStatus, OAuthErrorBody, PlanCode,
    RefreshedCredential, Session, SessionAvailability, SessionChange, SessionState,
    StoredCredential, TokenResponse, TokenWalletKind,
};
use oauth::{OAuthAuthority, canonical_user_id, validate_account};
use state::{
    SessionControl, authorization_headers_inner, current_plan, current_profile,
    ensure_session_loaded, install_session, local_session_status_inner, timestamp,
};

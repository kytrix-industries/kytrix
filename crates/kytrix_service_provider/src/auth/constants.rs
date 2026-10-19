pub const ACCOUNT_ORIGIN: &str = cfg_select! {
    feature = "production" => "https://account.kytrix.xyz",
    _ => "http://account.localhost",
};
pub const API_RESOURCE: &str = cfg_select! {
    feature = "production" => "https://api.kytrix.xyz/",
    _ => "http://api.localhost/",
};
pub const CLIENT_ID: &str = "kytrix-desktop";
pub const SCOPE: &str = "realtime";
pub const DASHBOARD_HANDOFF_PATH: &str = "v1/account/dashboard-handoff";

pub(super) const CREDENTIAL_SERVICE: &str = "xyz.kytrix.desktop";
pub(super) const CREDENTIAL_USER: &str = "oauth";
pub(super) const TOKEN_TTL_SECS: i64 = 600;
pub(super) const REFRESH_EARLY_SECS: i64 = 30;
// The server challenges 60 seconds before its authoritative token expiry. Keep
// enough margin for token issuance, transport, and local clock truncation.
pub(super) const RENEWAL_REFRESH_EARLY_SECS: i64 = 90;
pub(super) const MAX_RESPONSE_BYTES: usize = 16 * 1024;
// Windows Credential Manager is the narrowest supported keyring backend.
pub(super) const MAX_CREDENTIAL_BYTES: usize = 5 * 512;
pub(super) const AUTH_COMMAND_CAPACITY: usize = 1;

//! The credential set one live run reads, and the data centre it belongs to.
//!
//! The three values are read from the environment, or from the workspace root's
//! `longbridge-test.env`, or from the file `KYTRIX_LONGBRIDGE_TEST_CREDENTIALS` names.
//! No value ever reaches an assertion message, a print, or a log line: what this suite
//! states about a value is how long it is, or whether one of them carries the `us_`
//! prefix that decides the data centre (§4.1) — never the value itself.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use ahash::AHashMap;
use reqwest::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderName, HeaderValue};

/// The environment variable that names the OpenAPI application key.
pub const API_KEY_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_API_KEY";
/// The environment variable that names the OpenAPI application secret.
pub const APP_SECRET_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_APP_SECRET";
/// The environment variable that names the access token.
pub const ACCESS_TOKEN_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_ACCESS_TOKEN";
/// The environment variable that names a credentials file of its own.
pub const CREDENTIALS_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_CREDENTIALS";
/// The file the workspace root may hold the three values in.
pub const CREDENTIALS_FILE: &str = "longbridge-test.env";

/// The three values one OpenAPI credential set holds.
///
/// Nothing here prints a value: the type deliberately implements no `Debug`, and
/// the only thing this suite ever states about a value is how long it is or whether
/// it carries the `us_` prefix that decides the data centre (§4.1).
pub struct Credentials {
    pub app_key: String,
    pub app_secret: String,
    pub access_token: String,
}

impl Credentials {
    /// Loads the credential set the live run is to use.
    ///
    /// The environment comes first, the file second, and a file named by
    /// `KYTRIX_LONGBRIDGE_TEST_CREDENTIALS` replaces the default location. The
    /// default location is the workspace root, found by walking up from this
    /// crate's own manifest directory until a `Cargo.toml` states a workspace — a
    /// checkout can live anywhere, so no path is written down.
    pub fn load() -> Option<Self> {
        let file_values = match credentials_file_path() {
            Some(path) => read_credentials_file(&path),
            None => AHashMap::new(),
        };
        let read = |name: &str, from_file: &AHashMap<String, String>| -> Option<String> {
            match env::var(name) {
                Ok(value) if !value.trim().is_empty() => Some(value),
                _ => from_file
                    .get(name)
                    .cloned()
                    .filter(|value| !value.is_empty()),
            }
        };
        let credentials = Self {
            app_key: read(API_KEY_ENV, &file_values)?,
            app_secret: read(APP_SECRET_ENV, &file_values)?,
            access_token: read(ACCESS_TOKEN_ENV, &file_values)?,
        };
        Some(credentials)
    }

    /// The data centre these credentials are served from (§4.1).
    ///
    /// Any one of the three values starting with `us_` is a US credential; anything
    /// else is the gateway's Asian default. The answer is a fact about the values,
    /// so it is derived rather than configured.
    pub fn data_centre(&self) -> &'static str {
        let us = [&self.app_key, &self.app_secret, &self.access_token]
            .iter()
            .any(|value| value.starts_with("us_"));
        if us { "us" } else { "ap" }
    }

    /// The two headers every Longbridge request carries (§4.1).
    pub fn region_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en"));
        headers.insert(
            HeaderName::from_static("x-dc-region"),
            HeaderValue::from_static(self.data_centre()),
        );
        headers
    }
}

/// The path of the credential file, if the workspace states one.
fn credentials_file_path() -> Option<PathBuf> {
    if let Ok(path) = env::var(CREDENTIALS_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let root = workspace_root()?;
    let path = root.join(CREDENTIALS_FILE);
    path.is_file().then_some(path)
}

/// Finds the workspace root by walking up from this crate's manifest directory.
fn workspace_root() -> Option<PathBuf> {
    let mut directory = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let manifest = directory.join("Cargo.toml");
        if let Ok(text) = fs::read_to_string(&manifest)
            && text.contains("[workspace]")
        {
            return Some(directory);
        }
        if !directory.pop() {
            return None;
        }
    }
}

/// Reads one `KEY=value` file.
///
/// The values never leave this function's caller: what is returned is the map the
/// three lookups above read, and nothing in this suite ever prints one.
fn read_credentials_file(path: &Path) -> AHashMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return AHashMap::new();
    };
    let mut values = AHashMap::with_capacity(4);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            values.insert(key.trim().to_owned(), value.to_owned());
        }
    }
    values
}

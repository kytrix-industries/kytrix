use std::{
    env, fs,
    path::{Path, PathBuf},
};

use ahash::AHashMap;
use reqwest::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderName, HeaderValue};

pub const API_KEY_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_API_KEY";
pub const APP_SECRET_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_APP_SECRET";
pub const ACCESS_TOKEN_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_ACCESS_TOKEN";
pub const CREDENTIALS_ENV: &str = "KYTRIX_LONGBRIDGE_TEST_CREDENTIALS";
pub const CREDENTIALS_FILE: &str = "longbridge-test.env";

pub struct Credentials {
    pub app_key: String,
    pub app_secret: String,
    pub access_token: String,
}

impl Credentials {
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

    pub fn data_centre(&self) -> &'static str {
        let us = [&self.app_key, &self.app_secret, &self.access_token]
            .iter()
            .any(|value| value.starts_with("us_"));
        if us { "us" } else { "ap" }
    }

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

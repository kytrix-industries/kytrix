//! KDE `kioslaverc` proxy discovery.

use std::{collections::BTreeMap, path::PathBuf};

use crate::{
    error::NetworkError,
    system_proxy::linux::{
        endpoint::{host_has_port, parse_linux_host_port},
        settings::{LinuxProxyConfig, LinuxSettings, config_from_values},
    },
};

pub(super) fn kde_config_from_keys(
    keys: &BTreeMap<String, String>,
    get: impl Fn(&str) -> Option<String>,
) -> Option<LinuxProxyConfig> {
    let proxy_type = match keys.get("ProxyType").and_then(|value| value.parse().ok()) {
        Some(value) => value,
        None => return Some(LinuxProxyConfig::Invalid),
    };
    let pac_script = keys.get("Proxy Config Script").cloned().unwrap_or_default();
    match proxy_type {
        0 => Some(LinuxProxyConfig::Direct),
        2 => Some(LinuxProxyConfig::Pac {
            url: if pac_script.trim().is_empty() {
                None
            } else {
                Some(pac_script)
            },
        }),
        3 => Some(LinuxProxyConfig::Pac { url: None }),
        4 => {
            let resolve = |host_key: &str,
                           socks: bool|
             -> Result<Option<(String, u16)>, NetworkError> {
                let value = match keys.get(host_key).map(|value| value.trim()) {
                    Some("") | None => None,
                    Some(name) => Some(get(name).filter(|value| !value.trim().is_empty()).ok_or(
                        NetworkError::InvalidData {
                            operation: "parse KDE proxy endpoint".into(),
                            reason: "the named environment variable is unset".into(),
                        },
                    )?),
                };
                kde_endpoint_from_value(keys, host_key, socks, value)
            };
            let (Ok(http), Ok(https), Ok(socks)) = (
                resolve("httpProxy", false),
                resolve("httpsProxy", false),
                resolve("socksProxy", true),
            ) else {
                return Some(LinuxProxyConfig::Invalid);
            };
            config_from_values("manual", false, "", http, https, socks)
        }
        1 => {
            let (Ok(http), Ok(https), Ok(socks)) = (
                kde_endpoint(keys, "httpProxy", false),
                kde_endpoint(keys, "httpsProxy", false),
                kde_endpoint(keys, "socksProxy", true),
            ) else {
                return Some(LinuxProxyConfig::Invalid);
            };
            config_from_values("manual", false, "", http, https, socks)
        }
        _ => Some(LinuxProxyConfig::Invalid),
    }
}

/// Reads proxy settings straight out of a `kioslaverc` document.
#[cfg(test)]
pub(super) fn kde_config_from_kioslaverc(
    text: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Option<LinuxProxyConfig> {
    kde_config_from_keys(&kde_keys_from_text(text), get)
}

pub(super) fn merge_kde_keys(into: &mut BTreeMap<String, String>, text: &str) {
    for (key, value) in kde_keys_from_text(text) {
        into.insert(key, value);
    }
}

pub(super) fn kde_from_disk(get: impl Fn(&str) -> Option<String>) -> Option<LinuxSettings> {
    let mut keys = BTreeMap::new();
    let mut read_any = false;
    for path in kde_kioslaverc_paths(&get) {
        if let Ok(text) = std::fs::read_to_string(path) {
            read_any = true;
            merge_kde_keys(&mut keys, &text);
        }
    }
    if !read_any {
        return None;
    }
    let bypass = keys
        .get("NoProxyFor")
        .map(|value| crate::system_proxy::bypass::parse_bypass_list(value))
        .unwrap_or_default();
    Some(LinuxSettings {
        config: kde_config_from_keys(&keys, &get).unwrap_or(LinuxProxyConfig::Invalid),
        bypass,
    })
}

fn kde_keys_from_text(text: &str) -> BTreeMap<String, String> {
    let mut keys = BTreeMap::new();
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            in_section = section == "Proxy Settings";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        keys.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    keys
}

fn kde_endpoint(
    keys: &BTreeMap<String, String>,
    host_key: &str,
    socks: bool,
) -> Result<Option<(String, u16)>, NetworkError> {
    kde_endpoint_from_value(keys, host_key, socks, keys.get(host_key).cloned())
}

fn kde_endpoint_from_value(
    keys: &BTreeMap<String, String>,
    host_key: &str,
    socks: bool,
    host: Option<String>,
) -> Result<Option<(String, u16)>, NetworkError> {
    let prefix = host_key.trim_end_matches("Proxy");
    let port_keys = [format!("{prefix}Port"), format!("{host_key}Port")];
    let mut configured_port = None;
    for port_key in port_keys {
        let Some(raw) = keys.get(&port_key).map(|value| value.trim()) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let port = raw.parse::<u16>().map_err(|_| NetworkError::InvalidData {
            operation: "parse KDE proxy endpoint".into(),
            reason: "proxy port is invalid".into(),
        })?;
        if port == 0 {
            return Err(NetworkError::InvalidData {
                operation: "parse KDE proxy endpoint".into(),
                reason: "proxy port must be greater than zero".into(),
            });
        }
        configured_port.get_or_insert(port);
    }
    let Some(host) = host.filter(|value| !value.trim().is_empty()) else {
        return if configured_port.is_some() {
            Err(NetworkError::InvalidData {
                operation: "parse KDE proxy endpoint".into(),
                reason: "proxy port is configured without a host".into(),
            })
        } else {
            Ok(None)
        };
    };
    let combined = if host_has_port(&host) {
        host
    } else if let Some(port) = configured_port {
        format!("{host} {port}")
    } else {
        host
    };
    parse_linux_host_port(&combined, socks)
        .map(Some)
        .ok_or_else(|| NetworkError::InvalidData {
            operation: "parse KDE proxy endpoint".into(),
            reason: "proxy host or port is invalid".into(),
        })
}

fn kde_kioslaverc_paths(get: &impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let home = get("HOME").unwrap_or_default();
    if let Some(kdehome) = get("KDEHOME").filter(|value| !value.is_empty()) {
        return vec![PathBuf::from(kdehome).join("share/config/kioslaverc")];
    }
    let mut dirs = Vec::with_capacity(1);
    let config_home = get("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{home}/.config"));
    dirs.push(PathBuf::from(config_home));
    if let Some(config_dirs) = get("XDG_CONFIG_DIRS") {
        for dir in config_dirs.split(':').filter(|dir| !dir.is_empty()) {
            dirs.push(PathBuf::from(dir));
        }
    }
    dirs.reverse();
    dirs.into_iter().map(|dir| dir.join("kioslaverc")).collect()
}

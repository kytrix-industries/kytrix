//! GNOME `gsettings` proxy discovery.

use std::process::Command;

use super::settings::{LinuxProxyConfig, LinuxSettings, config_from_values};
use crate::{
    error::NetworkError,
    system_proxy::{
        bypass::parse_bypass_list,
        linux::{command::bounded_command_output, endpoint::parse_linux_host_port},
    },
};

pub(super) fn gnome_from_gsettings() -> Option<LinuxSettings> {
    gnome_from_get(|schema, key| gsettings_get(&[schema, key]))
}

pub(super) fn gnome_from_get(get: impl Fn(&str, &str) -> Option<String>) -> Option<LinuxSettings> {
    let mode = strip_gsettings(&get("org.gnome.system.proxy", "mode")?);
    if mode == "none" {
        return Some(LinuxSettings {
            config: LinuxProxyConfig::Direct,
            bypass: Vec::new(),
        });
    }
    if mode != "auto" && mode != "manual" {
        return Some(LinuxSettings {
            config: LinuxProxyConfig::Invalid,
            bypass: Vec::new(),
        });
    }

    let config = if mode == "manual" {
        let use_same =
            match gnome_use_same_from_value(get("org.gnome.system.proxy", "use-same-proxy")) {
                Ok(use_same) => use_same,
                Err(_) => {
                    return Some(LinuxSettings {
                        config: LinuxProxyConfig::Invalid,
                        bypass: Vec::new(),
                    });
                }
            };
        gnome_manual_config(
            use_same,
            gnome_endpoint_from_get(&get, "org.gnome.system.proxy.http", false),
            gnome_endpoint_from_get(&get, "org.gnome.system.proxy.https", false),
            gnome_endpoint_from_get(&get, "org.gnome.system.proxy.socks", true),
        )
    } else {
        let autoconfig = match gnome_autoconfig_url_from_value(get(
            "org.gnome.system.proxy",
            "autoconfig-url",
        )) {
            Ok(autoconfig) => autoconfig,
            Err(_) => {
                return Some(LinuxSettings {
                    config: LinuxProxyConfig::Invalid,
                    bypass: Vec::new(),
                });
            }
        };
        config_from_values(&mode, false, &autoconfig, None, None, None)?
    };
    let bypass =
        parse_gsettings_list(&get("org.gnome.system.proxy", "ignore-hosts").unwrap_or_default());
    Some(LinuxSettings { config, bypass })
}

pub(super) fn gnome_manual_config(
    use_same: bool,
    http: Result<Option<(String, u16)>, NetworkError>,
    https: Result<Option<(String, u16)>, NetworkError>,
    socks: Result<Option<(String, u16)>, NetworkError>,
) -> LinuxProxyConfig {
    let (Ok(http), Ok(https), Ok(socks)) = (http, https, socks) else {
        return LinuxProxyConfig::Invalid;
    };
    config_from_values("manual", use_same, "", http, https, socks)
        .unwrap_or(LinuxProxyConfig::Invalid)
}

pub(super) fn gnome_use_same_from_value(raw: Option<String>) -> Result<bool, NetworkError> {
    match strip_gsettings(&raw.ok_or_else(|| NetworkError::InvalidData {
        operation: "read GNOME proxy settings".into(),
        reason: "use-same-proxy is missing".into(),
    })?)
    .as_str()
    {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(NetworkError::InvalidData {
            operation: "read GNOME proxy settings".into(),
            reason: "use-same-proxy is invalid".into(),
        }),
    }
}

pub(super) fn gnome_autoconfig_url_from_value(raw: Option<String>) -> Result<String, NetworkError> {
    let raw = raw.ok_or_else(|| NetworkError::InvalidData {
        operation: "read GNOME proxy settings".into(),
        reason: "autoconfig-url is missing".into(),
    })?;
    let text = raw.trim();
    let value = text
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .or_else(|| {
            text.strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        })
        .ok_or_else(|| NetworkError::InvalidData {
            operation: "read GNOME proxy settings".into(),
            reason: "autoconfig-url is not quoted".into(),
        })?;
    if value.contains('\\') {
        return Err(NetworkError::InvalidData {
            operation: "read GNOME proxy settings".into(),
            reason: "autoconfig-url contains an escape sequence".into(),
        });
    }
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let parsed = url::Url::parse(value).map_err(|_| NetworkError::InvalidData {
        operation: "read GNOME proxy settings".into(),
        reason: "autoconfig-url is invalid".into(),
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(NetworkError::InvalidData {
            operation: "read GNOME proxy settings".into(),
            reason: "autoconfig-url must use HTTP or HTTPS".into(),
        });
    }
    Ok(value.to_owned())
}

fn gnome_endpoint_from_get(
    get: &impl Fn(&str, &str) -> Option<String>,
    schema: &str,
    socks: bool,
) -> Result<Option<(String, u16)>, NetworkError> {
    gnome_endpoint_from_values(get(schema, "host"), get(schema, "port"), socks)
}

pub(super) fn gnome_endpoint_from_values(
    host: Option<String>,
    port: Option<String>,
    socks: bool,
) -> Result<Option<(String, u16)>, NetworkError> {
    let host = strip_gsettings(&host.ok_or_else(|| NetworkError::InvalidData {
        operation: "read GNOME proxy endpoint".into(),
        reason: "proxy host is missing".into(),
    })?);
    if host.is_empty() {
        return Ok(None);
    }
    let port = strip_gsettings(&port.ok_or_else(|| NetworkError::InvalidData {
        operation: "read GNOME proxy endpoint".into(),
        reason: "proxy port is missing".into(),
    })?)
    .parse::<u16>()
    .map_err(|_| NetworkError::InvalidData {
        operation: "read GNOME proxy endpoint".into(),
        reason: "proxy port is invalid".into(),
    })?;
    if port == 0 {
        return Err(NetworkError::InvalidData {
            operation: "read GNOME proxy endpoint".into(),
            reason: "proxy port must be greater than zero".into(),
        });
    }
    parse_linux_host_port(&format!("{host}:{port}"), socks)
        .map(Some)
        .ok_or_else(|| NetworkError::InvalidData {
            operation: "read GNOME proxy endpoint".into(),
            reason: "proxy host or port is invalid".into(),
        })
}

fn gsettings_get(args: &[&str]) -> Option<String> {
    let mut command = Command::new("gsettings");
    command.arg("get").args(args);
    let output = bounded_command_output(&mut command)?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn strip_gsettings(raw: &str) -> String {
    let text = raw.trim();
    let text = text.strip_prefix("uint32 ").unwrap_or(text).trim();
    text.trim_matches('\'').trim_matches('"').to_owned()
}

fn parse_gsettings_list(raw: &str) -> Vec<String> {
    parse_bypass_list(
        &raw.trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .replace("', '", ",")
            .replace("\",\"", ","),
    )
}

//! Shared host/port and PAC-decision parsing.

#[cfg(target_os = "macos")]
use std::net::Ipv6Addr;

use app_config::network::{ConfigError, ResolvedProxy, RouteTarget, normalize_proxy_host};
use smol_str::SmolStr;

pub(super) fn http_system_proxy(
    endpoint: &str,
    target: &RouteTarget,
    tls_to_proxy: bool,
) -> Result<ResolvedProxy, ConfigError> {
    let (host, port) = split_host_port(endpoint, target)?;
    Ok(ResolvedProxy::Http {
        host: SmolStr::new(host),
        port,
        credentials: None,
        tls_to_proxy,
    })
}

/// Materializes a SOCKS5 hop for a host name, an IPv4 literal, or a bracketed
/// IPv6 literal.
///
/// The target host name always stays with the proxy to resolve, which is the
/// only form that survives a proxy without a route to the local address family.
pub(super) fn socks_system_proxy(
    endpoint: &str,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    let (host, port) = split_host_port(endpoint, target)?;
    let host = normalize_proxy_host(&host).ok_or(ConfigError::InvalidSystemProxy {
        target: target.clone(),
    })?;
    Ok(ResolvedProxy::Socks5 {
        host,
        port,
        credentials: None,
        resolve_dns_at_remote: true,
    })
}

#[cfg(target_os = "macos")]
pub(super) fn format_host_port(host: &str, port: u16) -> String {
    let host = host.trim();
    let bare_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if bare_host.parse::<Ipv6Addr>().is_ok() {
        format!("[{bare_host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub(super) fn split_host_port(
    endpoint: &str,
    target: &RouteTarget,
) -> Result<(String, u16), ConfigError> {
    let endpoint = endpoint.trim();
    if let Some(rest) = endpoint.strip_prefix('[') {
        let Some((host, port)) = rest.split_once("]:") else {
            return Err(ConfigError::InvalidSystemProxy {
                target: target.clone(),
            });
        };
        return parse_port(host, port, true, target);
    }
    let Some((host, port)) = endpoint.rsplit_once(':') else {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    };
    parse_port(host, port, false, target)
}

fn parse_port(
    host: &str,
    port: &str,
    bracketed: bool,
    target: &RouteTarget,
) -> Result<(String, u16), ConfigError> {
    let host = host.trim();
    let port = port.trim();
    if host.is_empty() || (!bracketed && host.contains(':')) {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| ConfigError::InvalidSystemProxy {
            target: target.clone(),
        })?;
    if port == 0 {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    }
    if bracketed {
        Ok((format!("[{host}]"), port))
    } else {
        Ok((host.to_owned(), port))
    }
}

/// Parses a PAC `FindProxyForURL` return list and takes the first usable hop.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn parse_pac_decision(
    raw: &str,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    }
    for hop in raw.split(';') {
        let hop = hop.trim();
        if hop.is_empty() {
            continue;
        }
        if hop.eq_ignore_ascii_case("direct") {
            return Ok(ResolvedProxy::NoProxy);
        }
        let Some((kind, endpoint)) = hop.split_once(char::is_whitespace) else {
            continue;
        };
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            continue;
        }
        let candidate = match kind.to_ascii_lowercase().as_str() {
            "proxy" | "http" => http_system_proxy(endpoint, target, false),
            "https" => http_system_proxy(endpoint, target, true),
            "socks5" | "socks5h" => socks_system_proxy(endpoint, target),
            "socks" | "socks4" | "socks4a" => continue,
            _ => continue,
        };
        if let Ok(proxy) = candidate {
            return Ok(proxy);
        }
    }
    Err(ConfigError::InvalidSystemProxy {
        target: target.clone(),
    })
}

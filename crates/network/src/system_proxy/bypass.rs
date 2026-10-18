//! Chromium-style proxy bypass matching (`ignore-hosts` / `no_proxy` / `<local>`).

use std::net::IpAddr;

use url::Url;

use crate::error::NetworkError;

const MAX_GLOB_STATES: usize = 1 << 20;

pub(super) fn destination_is_bypassed(destination: &Url, rules: &[String]) -> bool {
    let Some(host) = destination.host_str() else {
        return false;
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    let port = destination.port_or_known_default();
    rules.iter().any(|rule| host_matches_rule(host, port, rule))
}

pub(super) fn parse_bypass_list(raw: &str) -> Vec<String> {
    raw.split([',', ';', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != "[]")
        .map(|part| part.trim_matches('\'').trim_matches('"').to_owned())
        .filter(|part| !part.is_empty())
        .collect()
}

fn host_matches_rule(host: &str, destination_port: Option<u16>, rule: &str) -> bool {
    let rule = rule.trim();
    if rule.is_empty() {
        return false;
    }
    let Some((rule, rule_port)) = split_rule_port(rule) else {
        return false;
    };
    if rule_port.is_some() && rule_port != destination_port {
        return false;
    }
    if rule.eq_ignore_ascii_case("<local>") {
        return !host.contains('.');
    }
    if let Some((cidr_host, prefix)) = rule.split_once('/')
        && let Ok(prefix) = prefix.parse::<u8>()
        && let (Ok(host_ip), Ok(rule_ip)) = (host.parse::<IpAddr>(), cidr_host.parse::<IpAddr>())
    {
        return ip_in_cidr(host_ip, rule_ip, prefix);
    }
    if let Ok(rule_ip) = rule.parse::<IpAddr>()
        && let Ok(host_ip) = host.parse::<IpAddr>()
    {
        return host_ip == rule_ip;
    }
    wildcard_or_suffix(host, rule)
}

fn split_rule_port(rule: &str) -> Option<(&str, Option<u16>)> {
    if let Some((network, prefix)) = rule.split_once('/')
        && network.parse::<IpAddr>().is_ok()
        && prefix.parse::<u8>().is_ok()
    {
        return Some((rule, None));
    }
    if let Some(bracketed) = rule.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        return match suffix {
            "" => Some((host, None)),
            suffix => suffix
                .strip_prefix(':')?
                .parse::<u16>()
                .ok()
                .map(|port| (host, Some(port))),
        };
    }
    if rule.parse::<IpAddr>().is_ok() {
        return Some((rule, None));
    }
    if let Some((host, port)) = rule.rsplit_once(':') {
        if host.is_empty() {
            return None;
        }
        return port.parse::<u16>().ok().map(|port| (host, Some(port)));
    }
    Some((rule, None))
}

fn ip_in_cidr(host: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (host, network) {
        (IpAddr::V4(host), IpAddr::V4(network)) if prefix <= 32 => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            u32::from(host) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(host), IpAddr::V6(network)) if prefix <= 128 => {
            let host = u128::from(host);
            let network = u128::from(network);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            host & mask == network & mask
        }
        _ => false,
    }
}

fn wildcard_or_suffix(host: &str, rule: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let rule = rule.to_ascii_lowercase();
    if let Some(pattern) = rule.strip_prefix("*.") {
        return host == pattern || host.ends_with(&format!(".{pattern}"));
    }
    if rule.starts_with('.') {
        return host.ends_with(&rule) || host == rule.trim_start_matches('.');
    }
    if rule.contains('*') {
        return glob_match(&host, &rule).unwrap_or(false);
    }
    host == rule
}

pub(super) fn glob_match(value: &str, pattern: &str) -> Result<bool, NetworkError> {
    let value = value.as_bytes();
    let pattern = pattern.as_bytes();
    let states = value
        .len()
        .checked_add(1)
        .and_then(|rows| {
            pattern
                .len()
                .checked_add(1)
                .and_then(|columns| rows.checked_mul(columns))
        })
        .ok_or_else(|| NetworkError::LimitExceeded {
            operation: "proxy bypass glob match".into(),
            limit: MAX_GLOB_STATES,
        })?;
    if states > MAX_GLOB_STATES {
        return Err(NetworkError::LimitExceeded {
            operation: "proxy bypass glob match".into(),
            limit: MAX_GLOB_STATES,
        });
    }

    let mut previous = vec![false; pattern.len() + 1];
    previous[0] = true;
    for (index, byte) in pattern.iter().enumerate() {
        previous[index + 1] = *byte == b'*' && previous[index];
    }
    let mut current = vec![false; pattern.len() + 1];
    for value_byte in value {
        for (index, pattern_byte) in pattern.iter().enumerate() {
            current[index + 1] = match pattern_byte {
                b'*' => current[index] || previous[index + 1],
                b'?' => previous[index],
                byte => *byte == *value_byte && previous[index],
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(false);
    }
    Ok(previous[pattern.len()])
}

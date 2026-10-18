//! Desktop proxy endpoint parsing shared by the GNOME, KDE, and environment
//! readers.

use crate::error::NetworkError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinuxHop {
    Http { host: String, port: u16 },
    Socks { host: String, port: u16 },
}

pub(super) fn parse_linux_hop(raw: &str, default_socks: bool) -> Result<LinuxHop, NetworkError> {
    if is_socks4(raw) {
        return Err(NetworkError::InvalidData {
            operation: "parse Linux proxy hop".into(),
            reason: "SOCKS4 is unsupported".into(),
        });
    }
    let (host, port) =
        parse_linux_host_port(raw, default_socks).ok_or_else(|| NetworkError::InvalidData {
            operation: "parse Linux proxy hop".into(),
            reason: "proxy host or port is invalid".into(),
        })?;
    if default_socks || looks_like_socks(raw) {
        Ok(LinuxHop::Socks { host, port })
    } else {
        Ok(LinuxHop::Http { host, port })
    }
}

pub(super) fn host_has_port(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.contains(' ') {
        return true;
    }
    if trimmed.starts_with('[') {
        return trimmed.contains("]:");
    }
    if let Some((_, port)) = trimmed.rsplit_once(':') {
        return port.chars().all(|character| character.is_ascii_digit());
    }
    false
}

pub(super) fn parse_linux_host_port(raw: &str, default_socks: bool) -> Option<(String, u16)> {
    let mut host = raw.trim().to_owned();
    if host.is_empty() || host.starts_with("//:") {
        return None;
    }
    if is_socks4(&host) {
        return None;
    }
    if let Some(index) = host.find("://") {
        host = host[index + 3..].to_owned();
    }
    if let Some(index) = host.find('@') {
        host = host[index + 1..].to_owned();
    }
    if host.ends_with('/') {
        host.pop();
    }
    if let Some(space) = host.find(' ') {
        host.replace_range(space..=space, ":");
    }
    let default_port = if default_socks || looks_like_socks(raw) {
        1080
    } else {
        80
    };
    if host.starts_with('[') {
        let end = host.find(']')?;
        if end == 1 || !host[end + 1..].is_empty() && !host[end + 1..].starts_with(':') {
            return None;
        }
        let (inner, port) = if host[end + 1..].is_empty() {
            (host[1..end].to_owned(), default_port)
        } else {
            let port = host[end + 2..].parse().ok()?;
            (host[1..end].to_owned(), port)
        };
        if port == 0 || inner.contains(']') {
            return None;
        }
        return Some((format!("[{inner}]"), port));
    }
    if let Some((name, port)) = host.rsplit_once(':') {
        if name.is_empty() || name.contains(':') {
            return None;
        }
        let port = port.parse().ok()?;
        (port != 0).then(|| (name.to_owned(), port))
    } else if host.is_empty() {
        None
    } else {
        Some((host, default_port))
    }
}

fn starts_with_ascii_ignore_case(raw: &str, prefix: &str) -> bool {
    raw.get(..prefix.len())
        .is_some_and(|slice| slice.eq_ignore_ascii_case(prefix))
}

fn looks_like_socks(raw: &str) -> bool {
    let raw = raw.trim();
    starts_with_ascii_ignore_case(raw, "socks5h://")
        || starts_with_ascii_ignore_case(raw, "socks5://")
        || starts_with_ascii_ignore_case(raw, "socks://")
}

fn is_socks4(raw: &str) -> bool {
    let raw = raw.trim();
    starts_with_ascii_ignore_case(raw, "socks4://")
}

//! Linux desktop proxy settings and their evaluation for a destination.

use app_config::network::{ConfigError, ResolvedProxy, RouteTarget};
use url::Url;

use crate::system_proxy::{
    bypass::{destination_is_bypassed, parse_bypass_list},
    core::destination_is_local,
    linux::{
        endpoint::{LinuxHop, parse_linux_hop},
        gnome::gnome_from_gsettings,
        kde::kde_from_disk,
        wpad::discover_wpad_url,
    },
    pac::evaluate_pac_url,
    parse::{http_system_proxy, socks_system_proxy},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinuxDesktop {
    GnomeFamily,
    Kde,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinuxProxyConfig {
    Direct,
    Pac {
        url: Option<String>,
    },
    Manual {
        http: Option<(String, u16)>,
        https: Option<(String, u16)>,
        https_socks: Option<(String, u16)>,
        socks: Option<(String, u16)>,
        use_same: bool,
    },
    Invalid,
}

pub(super) struct LinuxSettings {
    pub config: LinuxProxyConfig,
    pub bypass: Vec<String>,
}

impl std::fmt::Debug for LinuxSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxSettings")
            .field("config", &self.config)
            .field("bypass", &self.bypass)
            .finish()
    }
}

pub(crate) struct Snapshot {
    settings: Option<LinuxSettings>,
    discovered_wpad_url: Option<String>,
    fingerprint: u64,
}

impl Snapshot {
    pub(crate) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

pub(crate) fn snapshot() -> Snapshot {
    let settings = discover_settings();
    let discovered_wpad_url = settings
        .as_ref()
        .is_some_and(|settings| matches!(settings.config, LinuxProxyConfig::Pac { url: None }))
        .then(discover_wpad_url)
        .flatten();
    let fingerprint = fingerprint_settings(&settings, discovered_wpad_url.as_deref());
    Snapshot {
        settings,
        discovered_wpad_url,
        fingerprint,
    }
}

pub(crate) fn read(
    snapshot: Snapshot,
    target: &RouteTarget,
    destination: &Url,
) -> Result<ResolvedProxy, ConfigError> {
    evaluate_settings(
        snapshot.settings,
        snapshot.discovered_wpad_url,
        destination,
        snapshot.fingerprint,
        target,
    )
}

pub(crate) fn fingerprint() -> u64 {
    snapshot().fingerprint()
}

pub(super) fn fingerprint_settings(
    settings: &Option<LinuxSettings>,
    discovered_wpad_url: Option<&str>,
) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};

    use ahash::RandomState;
    let mut hasher = RandomState::with_seeds(0, 0, 0, 0).build_hasher();
    format!("{settings:?}").hash(&mut hasher);
    if settings
        .as_ref()
        .is_some_and(|settings| matches!(settings.config, LinuxProxyConfig::Pac { url: None }))
    {
        discovered_wpad_url.hash(&mut hasher);
    }
    hasher.finish()
}

fn discover_settings() -> Option<LinuxSettings> {
    let get = |key: &str| std::env::var(key).ok();
    let desktop = linux_desktop_from_env(get);
    let settings = match desktop {
        LinuxDesktop::GnomeFamily => gnome_from_gsettings(),
        LinuxDesktop::Kde => kde_from_disk(get),
        LinuxDesktop::Other => None,
    };
    settings.or_else(|| linux_settings_from_env(get))
}

pub(super) fn evaluate_settings(
    settings: Option<LinuxSettings>,
    discovered_wpad_url: Option<String>,
    destination: &Url,
    fingerprint: u64,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    if destination_is_local(destination) {
        return Ok(ResolvedProxy::NoProxy);
    }
    let Some(settings) = settings else {
        return Ok(ResolvedProxy::NoProxy);
    };
    if destination_is_bypassed(destination, &settings.bypass) {
        return Ok(ResolvedProxy::NoProxy);
    }
    match settings.config {
        LinuxProxyConfig::Direct => Ok(ResolvedProxy::NoProxy),
        LinuxProxyConfig::Pac { url } => {
            let script_url = match url {
                Some(url) if !url.trim().is_empty() => url,
                _ => discovered_wpad_url.ok_or(ConfigError::InvalidSystemProxy {
                    target: target.clone(),
                })?,
            };
            evaluate_pac_url(&script_url, destination, fingerprint, target)
        }
        LinuxProxyConfig::Invalid => Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        }),
        LinuxProxyConfig::Manual {
            http,
            https,
            https_socks,
            socks,
            use_same,
        } => evaluate_manual(
            http,
            https,
            https_socks,
            socks,
            use_same,
            destination,
            target,
        ),
    }
}

fn evaluate_manual(
    http: Option<(String, u16)>,
    https: Option<(String, u16)>,
    https_socks: Option<(String, u16)>,
    socks: Option<(String, u16)>,
    use_same: bool,
    destination: &Url,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    if use_same {
        return match http {
            Some((host, port)) => http_system_proxy(&format!("{host}:{port}"), target, false),
            None => Ok(ResolvedProxy::NoProxy),
        };
    }
    let https_like = matches!(destination.scheme(), "https" | "wss");
    if https_like {
        if let Some((host, port)) = https {
            return http_system_proxy(&format!("{host}:{port}"), target, false);
        }
        if let Some((host, port)) = https_socks {
            return socks_system_proxy(&format!("{host}:{port}"), target);
        }
        if let Some((host, port)) = socks {
            return socks_system_proxy(&format!("{host}:{port}"), target);
        }
        return Ok(ResolvedProxy::NoProxy);
    }
    if let Some((host, port)) = http {
        return http_system_proxy(&format!("{host}:{port}"), target, false);
    }
    if let Some((host, port)) = socks {
        return socks_system_proxy(&format!("{host}:{port}"), target);
    }
    Ok(ResolvedProxy::NoProxy)
}

pub(super) fn linux_desktop_from_env(get: impl Fn(&str) -> Option<String>) -> LinuxDesktop {
    if let Some(xdg) = get("XDG_CURRENT_DESKTOP") {
        for part in xdg
            .split(':')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            match part {
                "KDE" => return LinuxDesktop::Kde,
                "GNOME" | "Unity" | "X-Cinnamon" | "Cinnamon" | "Deepin" | "DDE" | "Pantheon"
                | "UKUI" | "COSMIC" => return LinuxDesktop::GnomeFamily,
                "XFCE" | "LXQt" => return LinuxDesktop::Other,
                _ => {}
            }
        }
    }
    if get("KDE_FULL_SESSION").is_some() {
        return LinuxDesktop::Kde;
    }
    if get("GNOME_DESKTOP_SESSION_ID").is_some() {
        return LinuxDesktop::GnomeFamily;
    }
    if let Some(session) = get("DESKTOP_SESSION") {
        let session = session.to_ascii_lowercase();
        if session.contains("kde") {
            return LinuxDesktop::Kde;
        }
        if session.contains("gnome")
            || session.contains("cinnamon")
            || session.contains("unity")
            || session.contains("pantheon")
        {
            return LinuxDesktop::GnomeFamily;
        }
    }
    LinuxDesktop::Other
}

fn linux_settings_from_env(get: impl Fn(&str) -> Option<String>) -> Option<LinuxSettings> {
    let bypass = get("no_proxy")
        .or_else(|| get("NO_PROXY"))
        .map(|value| parse_bypass_list(&value))
        .unwrap_or_default();
    linux_config_from_env(get).map(|config| LinuxSettings { config, bypass })
}

pub(super) fn linux_config_from_env(
    get: impl Fn(&str) -> Option<String>,
) -> Option<LinuxProxyConfig> {
    if let Some(all_proxy) = get("all_proxy").filter(|value| !value.is_empty()) {
        return Some(match parse_linux_hop(&all_proxy, false) {
            Err(_) => LinuxProxyConfig::Invalid,
            Ok(LinuxHop::Http { host, port }) => LinuxProxyConfig::Manual {
                http: Some((host.clone(), port)),
                https: Some((host, port)),
                https_socks: None,
                socks: None,
                use_same: true,
            },
            Ok(LinuxHop::Socks { host, port }) => LinuxProxyConfig::Manual {
                http: None,
                https: None,
                https_socks: None,
                socks: Some((host, port)),
                use_same: false,
            },
        });
    }
    if let Some(auto_proxy) = get("auto_proxy") {
        if auto_proxy.is_empty() {
            return Some(LinuxProxyConfig::Direct);
        }
        return Some(LinuxProxyConfig::Pac {
            url: Some(auto_proxy),
        });
    }
    let mut http = None;
    let mut https = None;
    let mut https_socks = None;
    let mut socks = None;
    if let Some(value) = get("https_proxy").filter(|value| !value.is_empty()) {
        match parse_linux_hop(&value, false) {
            Err(_) => return Some(LinuxProxyConfig::Invalid),
            Ok(LinuxHop::Http { host, port }) => https = Some((host, port)),
            Ok(LinuxHop::Socks { host, port }) => https_socks = Some((host, port)),
        }
    }
    if let Some(value) = get("http_proxy").filter(|value| !value.is_empty()) {
        match parse_linux_hop(&value, false) {
            Err(_) => return Some(LinuxProxyConfig::Invalid),
            Ok(LinuxHop::Http { host, port }) => http = Some((host, port)),
            Ok(LinuxHop::Socks { host, port }) => socks = socks.or(Some((host, port))),
        }
    }
    if http.is_some() || https.is_some() || https_socks.is_some() || socks.is_some() {
        return Some(LinuxProxyConfig::Manual {
            http,
            https,
            https_socks,
            socks,
            use_same: false,
        });
    }
    if get("SOCKS_VERSION").as_deref() == Some("4")
        && get("SOCKS_SERVER")
            .filter(|value| !value.is_empty())
            .is_some()
    {
        return Some(LinuxProxyConfig::Invalid);
    }
    get("SOCKS_SERVER")
        .filter(|value| !value.is_empty())
        .map(|value| match parse_linux_hop(&value, true) {
            Ok(LinuxHop::Socks { host, port }) | Ok(LinuxHop::Http { host, port }) => {
                LinuxProxyConfig::Manual {
                    http: None,
                    https: None,
                    https_socks: None,
                    socks: Some((host, port)),
                    use_same: false,
                }
            }
            Err(_) => LinuxProxyConfig::Invalid,
        })
}

/// Assembles a GNOME-shaped manual/auto/PAC configuration from raw values.
///
/// The KDE reader reuses it: both desktops describe the same three modes over
/// the same endpoint fields.
pub(super) fn config_from_values(
    mode: &str,
    use_same_proxy: bool,
    autoconfig_url: &str,
    http: Option<(String, u16)>,
    https: Option<(String, u16)>,
    socks: Option<(String, u16)>,
) -> Option<LinuxProxyConfig> {
    match mode {
        "none" => Some(LinuxProxyConfig::Direct),
        "auto" => Some(LinuxProxyConfig::Pac {
            url: {
                let url = autoconfig_url.trim();
                if url.is_empty() {
                    None
                } else {
                    Some(url.to_owned())
                }
            },
        }),
        "manual" => {
            if http.is_none() && https.is_none() && socks.is_none() {
                return Some(LinuxProxyConfig::Invalid);
            }
            Some(LinuxProxyConfig::Manual {
                http,
                https,
                https_socks: None,
                socks,
                use_same: use_same_proxy,
            })
        }
        _ => Some(LinuxProxyConfig::Invalid),
    }
}

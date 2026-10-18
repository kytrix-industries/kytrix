//! WinHTTP system proxy evaluation (PAC, WPAD, manual, bypass).

use std::{
    hash::{BuildHasher, Hash, Hasher},
    sync::{Arc, Condvar, LazyLock, Mutex},
    thread,
    time::{Duration, Instant},
};

use app_config::network::{ConfigError, ResolvedProxy, RouteTarget};
use url::Url;
use windows::{
    Win32::{
        Foundation::HGLOBAL,
        Networking::WinHttp::{
            WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY,
            WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
            WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS,
            WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WINHTTP_PROXY_INFO, WinHttpCloseHandle,
            WinHttpDetectAutoProxyConfigUrl, WinHttpGetIEProxyConfigForCurrentUser,
            WinHttpGetProxyForUrl, WinHttpOpen, WinHttpSetTimeouts,
        },
    },
    core::{Free, PCWSTR, PWSTR},
};

use super::{
    bypass::{destination_is_bypassed, parse_bypass_list},
    core::{destination_is_local, pac_request_url},
    parse::{http_system_proxy, socks_system_proxy},
};
use crate::error::NetworkError;

const PAC_TIMEOUT_MS: i32 = 10_000;
const WPAD_DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(2);

static WPAD_DISCOVERY: LazyLock<Arc<WpadDiscoveryCoordinator>> =
    LazyLock::new(|| Arc::new(WpadDiscoveryCoordinator::new()));

pub(super) struct IeProxyConfig {
    pub auto_detect: bool,
    pub auto_config_url: String,
    pub proxy: String,
    pub bypass: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) enum WpadSnapshot {
    NotRequested,
    Discovered(String),
    Failed,
    TimedOut,
}

struct CachedWpadSnapshot {
    value: WpadSnapshot,
    completed_at: Instant,
}

#[derive(Default)]
struct WpadDiscoveryState {
    running: bool,
    cached: Option<CachedWpadSnapshot>,
}

pub(super) struct WpadDiscoveryCoordinator {
    state: Mutex<WpadDiscoveryState>,
    ready: Condvar,
}

impl WpadDiscoveryCoordinator {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(WpadDiscoveryState::default()),
            ready: Condvar::new(),
        }
    }

    fn finish(&self, value: WpadSnapshot) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.running = false;
        state.cached = Some(CachedWpadSnapshot {
            value,
            completed_at: Instant::now(),
        });
        drop(state);
        self.ready.notify_all();
    }
}

pub(super) struct Snapshot {
    ie: IeProxyConfig,
    wpad: WpadSnapshot,
    fingerprint: u64,
}

impl Snapshot {
    pub(super) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

pub(super) fn snapshot() -> Result<Snapshot, NetworkError> {
    let ie = ie_proxy_config()?;
    let wpad = wpad_snapshot_for_config(&ie, detect_wpad_config_url_bounded);
    let fingerprint = fingerprint_ie_config(&ie, &wpad);
    Ok(Snapshot {
        ie,
        wpad,
        fingerprint,
    })
}

pub(super) fn read(
    snapshot: Snapshot,
    target: &RouteTarget,
    destination: &Url,
) -> Result<ResolvedProxy, ConfigError> {
    evaluate_ie_config(&snapshot.ie, &snapshot.wpad, destination, target)
}

pub(super) fn fingerprint() -> u64 {
    snapshot().map_or(0, |snapshot| snapshot.fingerprint())
}

pub(super) fn fingerprint_ie_config(ie: &IeProxyConfig, wpad: &WpadSnapshot) -> u64 {
    use ahash::RandomState;
    let mut hasher = RandomState::with_seeds(0, 0, 0, 0).build_hasher();
    ie.auto_detect.hash(&mut hasher);
    ie.auto_config_url.hash(&mut hasher);
    ie.proxy.hash(&mut hasher);
    ie.bypass.hash(&mut hasher);
    wpad.hash(&mut hasher);
    hasher.finish()
}

pub(super) fn wpad_snapshot_for_config(
    ie: &IeProxyConfig,
    discover: impl FnOnce() -> WpadSnapshot,
) -> WpadSnapshot {
    if ie.auto_detect && ie.auto_config_url.trim().is_empty() {
        discover()
    } else {
        WpadSnapshot::NotRequested
    }
}

fn detect_wpad_config_url_bounded() -> WpadSnapshot {
    coordinate_wpad_discovery(
        Arc::clone(&WPAD_DISCOVERY),
        Duration::from_millis(PAC_TIMEOUT_MS as u64),
        detect_wpad_config_url,
    )
}

pub(super) fn coordinate_wpad_discovery(
    coordinator: Arc<WpadDiscoveryCoordinator>,
    timeout: Duration,
    discover: impl FnOnce() -> WpadSnapshot + Send + 'static,
) -> WpadSnapshot {
    let deadline = Instant::now() + timeout;
    let mut state = coordinator
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = state
        .cached
        .as_ref()
        .filter(|cached| cached.completed_at.elapsed() < WPAD_DISCOVERY_CACHE_TTL)
    {
        return cached.value.clone();
    }

    if !state.running {
        state.running = true;
        drop(state);
        let worker_coordinator = Arc::clone(&coordinator);
        let worker = thread::Builder::new()
            .name("kytrix-wpad-discovery".into())
            .spawn(move || {
                let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(discover))
                    .unwrap_or(WpadSnapshot::Failed);
                worker_coordinator.finish(value);
            });
        if worker.is_err() {
            coordinator.finish(WpadSnapshot::Failed);
        }
        state = coordinator
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    loop {
        if !state.running {
            return state
                .cached
                .as_ref()
                .map_or(WpadSnapshot::Failed, |cached| cached.value.clone());
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return WpadSnapshot::TimedOut;
        };
        let (next, wait) = coordinator
            .ready
            .wait_timeout(state, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state = next;
        if wait.timed_out() && state.running {
            return WpadSnapshot::TimedOut;
        }
    }
}

fn detect_wpad_config_url() -> WpadSnapshot {
    unsafe {
        let mut url = PWSTR::null();
        let result = WinHttpDetectAutoProxyConfigUrl(
            WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A,
            &mut url,
        );
        let value = pwstr_to_string(url);
        free_pwstr(url);
        if result.is_err() {
            return WpadSnapshot::Failed;
        }
        match value {
            Ok(value) if !value.trim().is_empty() => {
                WpadSnapshot::Discovered(value.trim().to_owned())
            }
            Ok(_) | Err(_) => WpadSnapshot::Failed,
        }
    }
}

/// Which Windows proxy decision answers for a destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IeRoutingMode {
    Automatic,
    Bypass,
    ManualProxy,
}

fn evaluate_ie_config(
    ie: &IeProxyConfig,
    wpad: &WpadSnapshot,
    destination: &Url,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    match ie_routing_mode(ie, destination) {
        IeRoutingMode::Automatic => {
            let config_url =
                automatic_config_url(ie, wpad).ok_or(ConfigError::InvalidSystemProxy {
                    target: target.clone(),
                })?;
            winhttp_proxy_for_url(&config_url, destination, target)
        }
        IeRoutingMode::Bypass => Ok(ResolvedProxy::NoProxy),
        IeRoutingMode::ManualProxy => parse_windows_proxy_server(&ie.proxy, destination, target),
    }
}

pub(super) fn automatic_config_url(ie: &IeProxyConfig, wpad: &WpadSnapshot) -> Option<String> {
    let explicit = ie.auto_config_url.trim();
    if !explicit.is_empty() {
        return Some(explicit.to_owned());
    }
    match wpad {
        WpadSnapshot::Discovered(url) => Some(url.clone()),
        WpadSnapshot::NotRequested | WpadSnapshot::Failed | WpadSnapshot::TimedOut => None,
    }
}

pub(super) fn ie_routing_mode(ie: &IeProxyConfig, destination: &Url) -> IeRoutingMode {
    if destination_is_local(destination) {
        IeRoutingMode::Bypass
    } else if ie.auto_detect || !ie.auto_config_url.trim().is_empty() {
        IeRoutingMode::Automatic
    } else if destination_is_bypassed(destination, &parse_bypass_list(&ie.bypass)) {
        IeRoutingMode::Bypass
    } else {
        IeRoutingMode::ManualProxy
    }
}

fn ie_proxy_config() -> Result<IeProxyConfig, NetworkError> {
    unsafe {
        let mut ie = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        WinHttpGetIEProxyConfigForCurrentUser(&mut ie).map_err(|error| NetworkError::Io {
            message: smol_str::format_smolstr!("{error}"),
        })?;
        let auto_config_url = pwstr_to_string(ie.lpszAutoConfigUrl);
        let proxy = pwstr_to_string(ie.lpszProxy);
        let bypass = pwstr_to_string(ie.lpszProxyBypass);
        free_pwstr(ie.lpszAutoConfigUrl);
        free_pwstr(ie.lpszProxy);
        free_pwstr(ie.lpszProxyBypass);
        Ok(IeProxyConfig {
            auto_detect: ie.fAutoDetect.as_bool(),
            auto_config_url: auto_config_url?,
            proxy: proxy?,
            bypass: bypass?,
        })
    }
}

fn winhttp_proxy_for_url(
    auto_config_url: &str,
    destination: &Url,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    let url = pac_request_url(destination);
    let url_wide: Vec<u16> = url
        .as_str()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let pac_wide: Vec<u16> = auto_config_url
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let session = WinHttpOpen(
            windows::core::w!("Kytrix"),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            Default::default(),
        );
        if session.is_null() {
            return Err(ConfigError::InvalidSystemProxy {
                target: target.clone(),
            });
        }
        let _ = WinHttpSetTimeouts(
            session,
            PAC_TIMEOUT_MS,
            PAC_TIMEOUT_MS,
            PAC_TIMEOUT_MS,
            PAC_TIMEOUT_MS,
        );
        let mut options = WINHTTP_AUTOPROXY_OPTIONS {
            dwFlags: WINHTTP_AUTOPROXY_CONFIG_URL,
            dwAutoDetectFlags: 0,
            lpszAutoConfigUrl: PCWSTR(pac_wide.as_ptr()),
            lpvReserved: std::ptr::null_mut(),
            dwReserved: 0,
            fAutoLogonIfChallenged: true.into(),
        };
        let mut info = WINHTTP_PROXY_INFO::default();
        let result =
            WinHttpGetProxyForUrl(session, PCWSTR(url_wide.as_ptr()), &mut options, &mut info);
        let access = info.dwAccessType;
        let proxy = pwstr_to_string(info.lpszProxy);
        free_pwstr(info.lpszProxy);
        free_pwstr(info.lpszProxyBypass);
        let _ = WinHttpCloseHandle(session);
        result.map_err(|_| ConfigError::InvalidSystemProxy {
            target: target.clone(),
        })?;
        if access == WINHTTP_ACCESS_TYPE_NO_PROXY {
            return Ok(ResolvedProxy::NoProxy);
        }
        if access == WINHTTP_ACCESS_TYPE_NAMED_PROXY {
            let proxy = proxy.map_err(|_| ConfigError::InvalidSystemProxy {
                target: target.clone(),
            })?;
            if proxy.trim().is_empty() {
                return Err(ConfigError::InvalidSystemProxy {
                    target: target.clone(),
                });
            }
            return parse_windows_proxy_server(&proxy, destination, target);
        }
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        })
    }
}

pub(super) fn parse_windows_proxy_server(
    server: &str,
    destination: &Url,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    let server = server.trim();
    if server.is_empty() {
        return Ok(ResolvedProxy::NoProxy);
    }
    if let Some(socks) = protocol_endpoint(server, "socks") {
        return socks_system_proxy(&socks, target);
    }
    let https_like = matches!(pac_request_url(destination).scheme(), "https");
    let mapped = if https_like {
        protocol_endpoint(server, "https").or_else(|| protocol_endpoint(server, "http"))
    } else {
        protocol_endpoint(server, "http")
    };
    if let Some(http) = mapped {
        return http_system_proxy(&http, target, false);
    }
    if server.contains('=') {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    }
    http_system_proxy(server, target, false)
}

fn protocol_endpoint(server: &str, protocol: &str) -> Option<String> {
    let prefix = format!("{protocol}=");
    server.split(';').find_map(|part| {
        part.trim()
            .strip_prefix(&prefix)
            .map(|endpoint| endpoint.trim().to_owned())
    })
}

unsafe fn pwstr_to_string(ptr: PWSTR) -> Result<String, NetworkError> {
    if ptr.is_null() {
        return Ok(String::new());
    }
    unsafe {
        ptr.to_string().map_err(|error| NetworkError::Io {
            message: smol_str::format_smolstr!("{error}"),
        })
    }
}

unsafe fn free_pwstr(ptr: PWSTR) {
    if !ptr.is_null() {
        unsafe {
            HGLOBAL(ptr.as_ptr() as *mut core::ffi::c_void).free();
        }
    }
}

//! Get-only operating-system proxy snapshot, evaluated for a destination URL.

use std::{
    net::IpAddr,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use app_config::network::{ConfigError, ResolvedProxy, RouteTarget};
use tokio_util::sync::CancellationToken;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

#[cfg(target_os = "linux")]
use super::linux;
#[cfg(target_os = "macos")]
use super::macos;
#[cfg(windows)]
use super::windows;

/// The URL a PAC script is asked about, with a WebSocket scheme mapped to its
/// HTTP equivalent.
pub(super) fn pac_request_url(destination: &Url) -> Url {
    let mut url = destination.clone();
    match url.scheme() {
        "wss" => {
            let _ = url.set_scheme("https");
        }
        "ws" => {
            let _ = url.set_scheme("http");
        }
        _ => {}
    }
    url
}

/// Whether a destination names a host on this machine.
///
/// These are Chromium's implicit bypass rules: a request to a local host — a
/// `localhost` name, a loopback address, a link-local address — is never handed
/// to a proxy, whatever the operating system is configured with. The
/// development service answers on `*.localhost`, so its routes stay direct even
/// on a machine that sends everything else through a proxy.
pub(super) fn destination_is_local(destination: &Url) -> bool {
    let Some(host) = destination.host_str() else {
        return false;
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if let Ok(address) = host.parse::<IpAddr>() {
        return match address {
            IpAddr::V4(address) => address.is_loopback() || address.is_link_local(),
            IpAddr::V6(address) => address.is_loopback() || address.is_unicast_link_local(),
        };
    }
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

const FINGERPRINT_TTL: Duration = Duration::from_secs(2);

pub(super) struct FingerprintCache {
    value: u64,
    checked_at: Instant,
    discovery_sequence: u64,
}

impl FingerprintCache {
    pub(super) fn new(value: u64, checked_at: Instant, discovery_sequence: u64) -> Self {
        Self {
            value,
            checked_at,
            discovery_sequence,
        }
    }

    pub(super) fn value(&self) -> u64 {
        self.value
    }

    pub(super) fn is_fresh(&self) -> bool {
        self.checked_at.elapsed() < FINGERPRINT_TTL
    }
}

static FINGERPRINT_CACHE: Mutex<Option<FingerprintCache>> = Mutex::new(None);
static FINGERPRINT_REFRESH_RUNNING: AtomicBool = AtomicBool::new(false);
static FINGERPRINT_DISCOVERY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) fn read_os_proxy(
    target: &RouteTarget,
    destination: &Url,
) -> Result<(ResolvedProxy, u64), ConfigError> {
    let discovery_sequence = next_discovery_sequence(&FINGERPRINT_DISCOVERY_SEQUENCE);
    #[cfg(windows)]
    {
        let snapshot = windows::snapshot().map_err(|_| ConfigError::InvalidSystemProxy {
            target: target.clone(),
        })?;
        let fingerprint = snapshot.fingerprint();
        publish_fingerprint(discovery_sequence, fingerprint);
        windows::read(snapshot, target, destination).map(|proxy| (proxy, fingerprint))
    }
    #[cfg(target_os = "linux")]
    {
        let snapshot = linux::snapshot();
        let fingerprint = snapshot.fingerprint();
        publish_fingerprint(discovery_sequence, fingerprint);
        linux::read(snapshot, target, destination).map(|proxy| (proxy, fingerprint))
    }
    #[cfg(target_os = "macos")]
    {
        let snapshot = macos::snapshot().ok_or(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        })?;
        let fingerprint = snapshot.fingerprint();
        publish_fingerprint(discovery_sequence, fingerprint);
        macos::read(snapshot, target, destination).map(|proxy| (proxy, fingerprint))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (destination, discovery_sequence);
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        })
    }
}

pub(crate) fn cached_os_proxy_fingerprint() -> u64 {
    FINGERPRINT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or(0, FingerprintCache::value)
}

pub(crate) async fn refresh_os_proxy_fingerprint(
    cancellation: CancellationToken,
    runtime: &Runtime,
) -> Option<u64> {
    if let Some(value) = fresh_cached_fingerprint(&FINGERPRINT_CACHE) {
        return Some(value);
    }
    if FINGERPRINT_REFRESH_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Some(cached_os_proxy_fingerprint());
    }
    // Reading the OS settings is a blocking call, and the caller may be a GPUI
    // task with no runtime context of its own: the runtime performs the read and
    // establishes the context for it.
    let task = runtime.clone().blocking(|| {
        struct RefreshGuard;
        impl Drop for RefreshGuard {
            fn drop(&mut self) {
                FINGERPRINT_REFRESH_RUNNING.store(false, Ordering::Release);
            }
        }
        let _guard = RefreshGuard;
        refresh_os_proxy_fingerprint_blocking()
    });
    tokio::select! {
        biased;
        () = cancellation.cancelled() => None,
        result = task => result.ok(),
    }
}

fn refresh_os_proxy_fingerprint_blocking() -> u64 {
    refresh_fingerprint(
        &FINGERPRINT_CACHE,
        &FINGERPRINT_DISCOVERY_SEQUENCE,
        platform_fingerprint,
    )
}

fn fresh_cached_fingerprint(cache: &Mutex<Option<FingerprintCache>>) -> Option<u64> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|cache| cache.is_fresh())
        .map(FingerprintCache::value)
}

pub(super) fn refresh_fingerprint(
    cache: &Mutex<Option<FingerprintCache>>,
    discovery_sequence: &AtomicU64,
    discover: impl FnOnce() -> u64,
) -> u64 {
    if let Some(value) = fresh_cached_fingerprint(cache) {
        return value;
    }
    let discovery_sequence = next_discovery_sequence(discovery_sequence);
    let value = discover();
    publish_fingerprint_to(cache, discovery_sequence, value)
}

fn next_discovery_sequence(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::Relaxed)
}

fn publish_fingerprint(discovery_sequence: u64, value: u64) {
    let _ = publish_fingerprint_to(&FINGERPRINT_CACHE, discovery_sequence, value);
}

fn publish_fingerprint_to(
    cache: &Mutex<Option<FingerprintCache>>,
    discovery_sequence: u64,
    value: u64,
) -> u64 {
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(current) = cache.as_ref()
        && current.discovery_sequence > discovery_sequence
    {
        return current.value();
    }
    *cache = Some(FingerprintCache::new(
        value,
        Instant::now(),
        discovery_sequence,
    ));
    value
}

fn platform_fingerprint() -> u64 {
    #[cfg(windows)]
    {
        windows::fingerprint()
    }
    #[cfg(target_os = "linux")]
    {
        linux::fingerprint()
    }
    #[cfg(target_os = "macos")]
    {
        macos::fingerprint()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

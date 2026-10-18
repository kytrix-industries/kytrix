//! CFNetwork system proxy evaluation (PAC, WPAD, scheme-specific static proxies).

use app_config::network::{ConfigError, ResolvedProxy, RouteTarget};
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
    url::CFURL,
};
use core_foundation_sys::{base::CFTypeRef, string::CFStringRef};
use url::Url;

use super::{
    core::{destination_is_local, pac_request_url},
    pac::evaluate_pac_url,
    parse::{format_host_port, http_system_proxy, socks_system_proxy},
};

#[link(name = "CFNetwork", kind = "framework")]
unsafe extern "C" {
    fn CFNetworkCopySystemProxySettings() -> CFTypeRef;
    fn CFNetworkCopyProxiesForURL(url: CFTypeRef, settings: CFTypeRef) -> CFTypeRef;
    static kCFProxyTypeKey: CFStringRef;
    static kCFProxyHostNameKey: CFStringRef;
    static kCFProxyPortNumberKey: CFStringRef;
    static kCFProxyAutoConfigurationURLKey: CFStringRef;
    static kCFProxyTypeNone: CFStringRef;
    static kCFProxyTypeHTTP: CFStringRef;
    static kCFProxyTypeHTTPS: CFStringRef;
    static kCFProxyTypeSOCKS: CFStringRef;
    static kCFProxyTypeAutoConfigurationURL: CFStringRef;
}

pub(super) struct Snapshot {
    settings: CFDictionary<CFString, CFType>,
    fingerprint: u64,
}

impl Snapshot {
    pub(super) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

pub(super) fn snapshot() -> Option<Snapshot> {
    let settings = system_proxy_settings()?;
    let fingerprint = fingerprint_settings(&settings);
    Some(Snapshot {
        settings,
        fingerprint,
    })
}

pub(super) fn read(
    snapshot: Snapshot,
    target: &RouteTarget,
    destination: &Url,
) -> Result<ResolvedProxy, ConfigError> {
    if destination_is_local(destination) {
        return Ok(ResolvedProxy::NoProxy);
    }
    let url = pac_request_url(destination);
    let proxies =
        proxies_for_url(&url, &snapshot.settings).ok_or(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        })?;
    first_usable_proxy(&proxies, destination, snapshot.fingerprint, target)
}

pub(super) fn fingerprint() -> u64 {
    snapshot().map_or(0, |snapshot| snapshot.fingerprint())
}

fn fingerprint_settings(settings: &CFDictionary<CFString, CFType>) -> u64 {
    use ahash::RandomState;
    RandomState::with_seeds(0, 0, 0, 0).hash_one(format!("{settings:?}"))
}

fn system_proxy_settings() -> Option<CFDictionary<CFString, CFType>> {
    unsafe {
        let raw = CFNetworkCopySystemProxySettings();
        if raw.is_null() {
            return None;
        }
        Some(CFDictionary::wrap_under_create_rule(raw as _))
    }
}

fn proxies_for_url(
    url: &Url,
    settings: &CFDictionary<CFString, CFType>,
) -> Option<CFArray<CFDictionary<CFString, CFType>>> {
    let cf_url = CFURL::from_path(url.as_str(), false)?;
    unsafe {
        let raw = CFNetworkCopyProxiesForURL(
            cf_url.as_concrete_TypeRef() as _,
            settings.as_concrete_TypeRef() as _,
        );
        if raw.is_null() {
            return None;
        }
        Some(CFArray::wrap_under_create_rule(raw as _))
    }
}

fn first_usable_proxy(
    proxies: &CFArray<CFDictionary<CFString, CFType>>,
    destination: &Url,
    fingerprint: u64,
    target: &RouteTarget,
) -> Result<ResolvedProxy, ConfigError> {
    let Some(dictionary) = proxies.iter().next() else {
        return Ok(ResolvedProxy::NoProxy);
    };
    let proxy_type = dictionary_string(&dictionary, unsafe { kCFProxyTypeKey }).ok_or(
        ConfigError::InvalidSystemProxy {
            target: target.clone(),
        },
    )?;
    if cf_string_equals(&proxy_type, unsafe { kCFProxyTypeNone }) {
        return Ok(ResolvedProxy::NoProxy);
    }
    if cf_string_equals(&proxy_type, unsafe { kCFProxyTypeAutoConfigurationURL }) {
        let pac = dictionary_string(&dictionary, unsafe { kCFProxyAutoConfigurationURLKey })
            .ok_or(ConfigError::InvalidSystemProxy {
                target: target.clone(),
            })?;
        return evaluate_pac_url(&pac, destination, fingerprint, target);
    }
    let host = dictionary_string(&dictionary, unsafe { kCFProxyHostNameKey }).ok_or(
        ConfigError::InvalidSystemProxy {
            target: target.clone(),
        },
    )?;
    let port = dictionary_number(&dictionary, unsafe { kCFProxyPortNumberKey }).ok_or(
        ConfigError::InvalidSystemProxy {
            target: target.clone(),
        },
    )?;
    if host.is_empty() || port == 0 {
        return Err(ConfigError::InvalidSystemProxy {
            target: target.clone(),
        });
    }
    let endpoint = format_host_port(&host, port);
    if cf_string_equals(&proxy_type, unsafe { kCFProxyTypeSOCKS }) {
        return socks_system_proxy(&endpoint, target);
    }
    if cf_string_equals(&proxy_type, unsafe { kCFProxyTypeHTTPS }) {
        return http_system_proxy(&endpoint, target, false);
    }
    if cf_string_equals(&proxy_type, unsafe { kCFProxyTypeHTTP }) {
        return http_system_proxy(&endpoint, target, false);
    }
    Err(ConfigError::InvalidSystemProxy {
        target: target.clone(),
    })
}

fn dictionary_string(
    dictionary: &CFDictionary<CFString, CFType>,
    key: CFStringRef,
) -> Option<String> {
    unsafe {
        let key = CFString::wrap_under_get_rule(key);
        dictionary
            .find(&key)
            .and_then(|value| value.downcast::<CFString>())
            .map(|string| string.to_string())
    }
}

fn dictionary_number(dictionary: &CFDictionary<CFString, CFType>, key: CFStringRef) -> Option<u16> {
    unsafe {
        let key = CFString::wrap_under_get_rule(key);
        dictionary
            .find(&key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|number| number.to_i64())
            .and_then(|value| u16::try_from(value).ok())
    }
}

fn cf_string_equals(value: &str, expected: CFStringRef) -> bool {
    // `kCFProxyType*` are process-wide constants, so wrapping one under the get
    // rule borrows a string that outlives the comparison.
    let expected = unsafe { CFString::wrap_under_get_rule(expected) };
    expected == value
}

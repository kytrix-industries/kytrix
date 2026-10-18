#[cfg(any(target_os = "linux", target_os = "macos"))]
use app_config::network::ConfigError;
use app_config::network::{KytrixServiceCapability, ResolvedProxy, RouteTarget, ServiceTarget};

use crate::system_proxy::parse::{http_system_proxy, socks_system_proxy};

#[cfg(not(target_os = "macos"))]
fn format_host_port(host: &str, port: u16) -> String {
    let host = host.trim();
    let bare_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if bare_host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{bare_host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(target_os = "macos")]
use crate::system_proxy::parse::format_host_port;

fn target() -> RouteTarget {
    RouteTarget::Service(ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Account,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn pac_decision_takes_first_usable_hop() {
    use crate::system_proxy::parse::parse_pac_decision;

    let target = target();
    assert_eq!(
        parse_pac_decision("DIRECT", &target),
        Ok(ResolvedProxy::NoProxy)
    );
    assert_eq!(
        parse_pac_decision("PROXY 127.0.0.1:8080", &target),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("127.0.0.1"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        parse_pac_decision("HTTPS proxy.example:443", &target),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("proxy.example"),
            port: 443,
            credentials: None,
            tls_to_proxy: true,
        })
    );
    assert_eq!(
        parse_pac_decision("SOCKS4 10.0.0.1:1080; PROXY 127.0.0.1:8080", &target),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("127.0.0.1"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        parse_pac_decision(
            "SOCKS5 proxy.example:1080; PROXY malformed; HTTPS proxy.example:443",
            &target,
        ),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("proxy.example"),
            port: 443,
            credentials: None,
            tls_to_proxy: true,
        })
    );
    assert_eq!(
        parse_pac_decision("PROXY; UNKNOWN value; DIRECT", &target),
        Ok(ResolvedProxy::NoProxy)
    );
    assert_eq!(
        parse_pac_decision("SOCKS4 10.0.0.1:1080; PROXY malformed", &target),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn http_proxy_endpoints_accept_bracketed_ipv6() {
    use crate::system_proxy::parse::parse_pac_decision;

    let target = target();
    assert_eq!(
        parse_pac_decision("PROXY [::1]:8080", &target),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("[::1]"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        parse_pac_decision("SOCKS5 [::1]:1080", &target),
        Ok(ResolvedProxy::Socks5 {
            host: smol_str::SmolStr::new_static("[::1]"),
            port: 1080,
            credentials: None,
            resolve_dns_at_remote: true,
        })
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn socks_proxy_endpoints_accept_host_names_and_both_ip_families() {
    use crate::system_proxy::parse::parse_pac_decision;

    let target = target();
    for (decision, host) in [
        ("SOCKS5 proxy.example:1080", "proxy.example"),
        ("SOCKS5 127.0.0.1:1080", "127.0.0.1"),
        ("SOCKS5H [2001:db8::1]:1080", "[2001:db8::1]"),
    ] {
        assert_eq!(
            parse_pac_decision(decision, &target),
            Ok(ResolvedProxy::Socks5 {
                host: smol_str::SmolStr::new(host),
                port: 1080,
                credentials: None,
                resolve_dns_at_remote: true,
            }),
            "{decision}"
        );
    }
    assert_eq!(
        parse_pac_decision("SOCKS5 bad host:1080", &target),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
}

#[test]
fn macos_style_bare_ipv6_proxy_hosts_are_bracketed_by_protocol() {
    let target = target();
    let endpoint = format_host_port("::1", 8080);

    assert_eq!(endpoint, "[::1]:8080");
    assert_eq!(
        http_system_proxy(&endpoint, &target, false),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("[::1]"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        socks_system_proxy(&endpoint, &target),
        Ok(ResolvedProxy::Socks5 {
            host: smol_str::SmolStr::new_static("[::1]"),
            port: 8080,
            credentials: None,
            resolve_dns_at_remote: true,
        })
    );
}

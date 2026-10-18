use std::{
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use app_config::network::{
    ConfigError, KytrixServiceCapability, ResolvedProxy, RouteTarget, ServiceTarget,
};
use smol_str::SmolStr;
use url::Url;

use crate::system_proxy::windows::{
    IeProxyConfig, IeRoutingMode, WpadDiscoveryCoordinator, WpadSnapshot, automatic_config_url,
    coordinate_wpad_discovery, fingerprint_ie_config, ie_routing_mode, parse_windows_proxy_server,
    wpad_snapshot_for_config,
};

fn target() -> RouteTarget {
    RouteTarget::Service(ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Account,
    })
}

fn dest(raw: &str) -> Url {
    Url::parse(raw).unwrap()
}

fn ie_config(auto_detect: bool, auto_config_url: &str) -> IeProxyConfig {
    IeProxyConfig {
        auto_detect,
        auto_config_url: auto_config_url.to_owned(),
        proxy: "proxy.example:8080".to_owned(),
        bypass: "api.example".to_owned(),
    }
}

#[test]
fn windows_proxy_server_parses_host_names_and_prefers_socks() {
    let target = target();
    let https = dest("https://api.example/v1");
    assert_eq!(
        parse_windows_proxy_server("", &https, &target),
        Ok(ResolvedProxy::NoProxy)
    );
    assert_eq!(
        parse_windows_proxy_server("127.0.0.1:7890", &https, &target),
        Ok(ResolvedProxy::Http {
            host: SmolStr::new_static("127.0.0.1"),
            port: 7890,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        parse_windows_proxy_server("proxy.corp.example:8080", &https, &target),
        Ok(ResolvedProxy::Http {
            host: SmolStr::new_static("proxy.corp.example"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        parse_windows_proxy_server("socks=127.0.0.1:7891", &https, &target),
        Ok(ResolvedProxy::Socks5 {
            host: SmolStr::new_static("127.0.0.1"),
            port: 7891,
            credentials: None,
            resolve_dns_at_remote: true,
        })
    );
    assert_eq!(
        parse_windows_proxy_server("http=127.0.0.1:7890;socks=127.0.0.1:7891", &https, &target),
        Ok(ResolvedProxy::Socks5 {
            host: SmolStr::new_static("127.0.0.1"),
            port: 7891,
            credentials: None,
            resolve_dns_at_remote: true,
        })
    );
    assert_eq!(
        parse_windows_proxy_server(
            "https=https.proxy:8443;http=http.proxy:8080",
            &https,
            &target,
        ),
        Ok(ResolvedProxy::Http {
            host: SmolStr::new_static("https.proxy"),
            port: 8443,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        parse_windows_proxy_server(
            "https=https.proxy:8443;http=http.proxy:8080",
            &dest("http://api.example/v1"),
            &target,
        ),
        Ok(ResolvedProxy::Http {
            host: SmolStr::new_static("http.proxy"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        parse_windows_proxy_server("socks=proxy.example:7891", &https, &target),
        Ok(ResolvedProxy::Socks5 {
            host: SmolStr::new_static("proxy.example"),
            port: 7891,
            credentials: None,
            resolve_dns_at_remote: true,
        })
    );
    assert_eq!(
        parse_windows_proxy_server("socks=[::1]:7891", &https, &target),
        Ok(ResolvedProxy::Socks5 {
            host: SmolStr::new_static("[::1]"),
            port: 7891,
            credentials: None,
            resolve_dns_at_remote: true,
        })
    );
    assert_eq!(
        parse_windows_proxy_server("socks=bad host:7891", &https, &target),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
}

#[test]
fn automatic_windows_proxy_does_not_apply_the_manual_bypass_list() {
    let destination = dest("https://api.example/v1");

    assert_eq!(
        ie_routing_mode(&ie_config(true, ""), &destination),
        IeRoutingMode::Automatic
    );
    assert_eq!(
        ie_routing_mode(&ie_config(false, "http://wpad/proxy.pac"), &destination),
        IeRoutingMode::Automatic
    );
    assert_eq!(
        ie_routing_mode(&ie_config(false, ""), &destination),
        IeRoutingMode::Bypass
    );
}

#[test]
fn windows_never_answers_a_local_destination_with_a_proxy() {
    let local = dest("http://account.localhost/oauth/token");

    for ie in [
        ie_config(true, ""),
        ie_config(false, "http://wpad/proxy.pac"),
        ie_config(false, ""),
    ] {
        assert_eq!(
            ie_routing_mode(&ie, &local),
            IeRoutingMode::Bypass,
            "{local}"
        );
    }
}

#[test]
fn windows_wpad_snapshot_changes_the_route_fingerprint() {
    let ie = ie_config(true, "");
    let first = WpadSnapshot::Discovered("http://wpad.first/wpad.dat".into());
    let second = WpadSnapshot::Discovered("http://wpad.second/wpad.dat".into());

    assert_ne!(
        fingerprint_ie_config(&ie, &first),
        fingerprint_ie_config(&ie, &second)
    );
    assert_ne!(
        fingerprint_ie_config(&ie, &first),
        fingerprint_ie_config(&ie, &WpadSnapshot::Failed)
    );
    assert_eq!(
        automatic_config_url(&ie, &first).as_deref(),
        Some("http://wpad.first/wpad.dat")
    );
}

#[test]
fn windows_manual_skips_wpad_and_explicit_pac_is_preserved() {
    let manual = ie_config(false, "");
    assert_eq!(
        wpad_snapshot_for_config(&manual, || panic!("manual mode must not discover WPAD")),
        WpadSnapshot::NotRequested
    );

    let explicit = ie_config(true, "http://explicit/proxy.pac");
    assert_eq!(
        wpad_snapshot_for_config(&explicit, || panic!(
            "an explicit PAC URL must not discover WPAD"
        )),
        WpadSnapshot::NotRequested
    );
    assert_eq!(
        automatic_config_url(
            &explicit,
            &WpadSnapshot::Discovered("http://wpad/network.pac".into())
        )
        .as_deref(),
        Some("http://explicit/proxy.pac")
    );
}

#[test]
fn concurrent_wpad_callers_share_one_discovery_result() {
    const CALLERS: usize = 6;
    let coordinator = Arc::new(WpadDiscoveryCoordinator::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(CALLERS + 1));
    let mut callers = Vec::with_capacity(CALLERS);
    for _ in 0..CALLERS {
        let coordinator = Arc::clone(&coordinator);
        let calls = Arc::clone(&calls);
        let start = Arc::clone(&start);
        callers.push(std::thread::spawn(move || {
            start.wait();
            coordinate_wpad_discovery(coordinator, Duration::from_secs(1), move || {
                calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                WpadSnapshot::Discovered("http://wpad/shared.dat".into())
            })
        }));
    }
    start.wait();

    for caller in callers {
        assert_eq!(
            caller.join().expect("the caller must not panic"),
            WpadSnapshot::Discovered("http://wpad/shared.dat".into())
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn timed_out_wpad_waiters_do_not_start_another_worker() {
    let coordinator = Arc::new(WpadDiscoveryCoordinator::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_sender, started_receiver) = mpsc::sync_channel(1);
    let first_coordinator = Arc::clone(&coordinator);
    let first_calls = Arc::clone(&calls);
    let first_gate = Arc::clone(&gate);
    let first = std::thread::spawn(move || {
        coordinate_wpad_discovery(first_coordinator, Duration::from_millis(50), move || {
            first_calls.fetch_add(1, Ordering::SeqCst);
            started_sender
                .send(())
                .expect("the caller is still waiting");
            let (lock, ready) = &*first_gate;
            let guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _guard = ready
                .wait_while(guard, |released| !*released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            WpadSnapshot::Discovered("http://wpad/after-timeout.dat".into())
        })
    });
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("the discovery worker must start");
    assert_eq!(
        first.join().expect("the first caller must not panic"),
        WpadSnapshot::TimedOut
    );

    let second_calls = Arc::clone(&calls);
    assert_eq!(
        coordinate_wpad_discovery(
            Arc::clone(&coordinator),
            Duration::from_millis(20),
            move || {
                second_calls.fetch_add(1, Ordering::SeqCst);
                WpadSnapshot::Failed
            },
        ),
        WpadSnapshot::TimedOut
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (lock, ready) = &*gate;
    *lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    ready.notify_all();
    let final_calls = Arc::clone(&calls);
    assert_eq!(
        coordinate_wpad_discovery(coordinator, Duration::from_secs(1), move || {
            final_calls.fetch_add(1, Ordering::SeqCst);
            WpadSnapshot::Failed
        }),
        WpadSnapshot::Discovered("http://wpad/after-timeout.dat".into())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

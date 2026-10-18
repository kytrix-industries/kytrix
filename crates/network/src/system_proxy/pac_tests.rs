use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use ahash::AHashMap;
use app_config::network::{ConfigError, KytrixServiceCapability, ResolvedProxy, ServiceTarget};
use url::Url;

use crate::{
    error::NetworkError,
    system_proxy::pac::{
        DnsLookupCoordinator, PAC_DNS_MAX_CALLS, PAC_MAX_BYTES, PacDnsBudget,
        bounded_dns_lookup_with, dns_domain_matches, evaluate_pac_script,
        local_host_or_domain_matches, read_bounded_pac_body, sh_exp_match_glob,
    },
};

fn target() -> RouteTarget {
    RouteTarget::Service(ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Account,
    })
}

fn url(raw: &str) -> Url {
    Url::parse(raw).unwrap()
}

#[test]
fn read_bounded_pac_body_stops_at_the_limit() {
    let allowed = vec![b'x'; 32];
    assert_eq!(
        read_bounded_pac_body(allowed.as_slice()).unwrap(),
        "x".repeat(32)
    );
    let oversized = vec![b'x'; PAC_MAX_BYTES + 1];
    assert!(read_bounded_pac_body(oversized.as_slice()).is_err());
}

#[test]
fn sh_exp_match_handles_wildcards_without_recursive_backtracking() {
    assert_eq!(
        sh_exp_match_glob("https://api.example/v1", "*://*.example/*"),
        Ok(true)
    );
    assert_eq!(sh_exp_match_glob("file1.txt", "file?.txt"), Ok(true));
    assert_eq!(sh_exp_match_glob("file10.txt", "file?.txt"), Ok(false));

    let pattern = format!("{}b", "*a".repeat(10_000));
    let value = format!("{}c", "a".repeat(10_000));
    assert!(matches!(
        sh_exp_match_glob(&value, &pattern),
        Err(NetworkError::LimitExceeded { .. })
    ));
}

#[test]
fn dns_domain_is_matches_standard_suffix_shapes() {
    assert!(dns_domain_matches("corp.example", "corp.example"));
    assert!(dns_domain_matches("api.corp.example", "corp.example"));
    assert!(dns_domain_matches("api.corp.example", ".corp.example"));
    assert!(!dns_domain_matches("corp.example", ".corp.example"));
    assert!(!dns_domain_matches(
        "api.corp.example.test",
        ".corp.example"
    ));
}

#[test]
fn local_host_or_domain_is_requires_plain_host_for_prefix_matching() {
    assert!(local_host_or_domain_matches("api", "api.corp.example"));
    assert!(local_host_or_domain_matches("api.corp", "api.corp"));
    assert!(!local_host_or_domain_matches(
        "api.corp",
        "api.corp.example"
    ));
    assert!(local_host_or_domain_matches("api", "api.example"));
    assert!(!local_host_or_domain_matches("api", "other.example"));
}

#[test]
fn dns_lookup_is_bounded_and_single_flight_after_timeout() {
    let coordinator = Arc::new(DnsLookupCoordinator::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_sender, started_receiver) = mpsc::sync_channel(1);
    let first_coordinator = Arc::clone(&coordinator);
    let first_calls = Arc::clone(&calls);
    let first_gate = Arc::clone(&gate);
    let first = thread::spawn(move || {
        bounded_dns_lookup_with(
            first_coordinator,
            "stalled.example".into(),
            Duration::from_millis(40),
            move |_| {
                first_calls.fetch_add(1, Ordering::SeqCst);
                started_sender.send(()).expect("the waiter remains active");
                let (lock, ready) = &*first_gate;
                let guard = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _guard = ready
                    .wait_while(guard, |released| !*released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Some("127.0.0.1".into())
            },
        )
    });
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("the lookup worker must start");
    assert_eq!(first.join().expect("the first waiter must not panic"), None);

    let second_calls = Arc::clone(&calls);
    assert_eq!(
        bounded_dns_lookup_with(
            Arc::clone(&coordinator),
            "stalled.example".into(),
            Duration::from_millis(20),
            move |_| {
                second_calls.fetch_add(1, Ordering::SeqCst);
                Some("192.0.2.1".into())
            },
        ),
        None
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (lock, ready) = &*gate;
    *lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    ready.notify_all();
    let final_calls = Arc::clone(&calls);
    assert_eq!(
        bounded_dns_lookup_with(
            coordinator,
            "stalled.example".into(),
            Duration::from_secs(1),
            move |_| {
                final_calls.fetch_add(1, Ordering::SeqCst);
                Some("192.0.2.2".into())
            },
        ),
        Some("127.0.0.1".into())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn repeated_dns_helpers_share_one_total_budget() {
    let started_at = std::time::Instant::now();
    let mut budget = PacDnsBudget {
        calls: 0,
        deadline: started_at + Duration::from_millis(1500),
        cache: AHashMap::with_capacity(PAC_DNS_MAX_CALLS),
    };

    assert_eq!(
        budget.next_timeout(started_at),
        Some(Duration::from_secs(1))
    );
    assert_eq!(
        budget.next_timeout(started_at + Duration::from_secs(1)),
        Some(Duration::from_millis(500))
    );
    assert_eq!(
        budget.next_timeout(started_at + Duration::from_millis(1500)),
        None
    );
    assert_eq!(
        budget.next_timeout(started_at + Duration::from_millis(1501)),
        None
    );

    budget.calls = PAC_DNS_MAX_CALLS;
    assert_eq!(budget.next_timeout(started_at), None);
}

#[test]
fn pac_script_returns_direct_and_proxy() {
    let target = target();
    let destination = url("https://api.example/v1");
    assert_eq!(
        evaluate_pac_script(
            "function FindProxyForURL(url, host) { return \"DIRECT\"; }",
            &destination,
            &target,
        ),
        Ok(ResolvedProxy::NoProxy)
    );
    assert_eq!(
        evaluate_pac_script(
            "function FindProxyForURL(url, host) { return \"PROXY 127.0.0.1:8080\"; }",
            &destination,
            &target,
        ),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("127.0.0.1"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        evaluate_pac_script(
            "function FindProxyForURL(url, host) { return \"HTTPS proxy.example:443\"; }",
            &destination,
            &target,
        ),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("proxy.example"),
            port: 443,
            credentials: None,
            tls_to_proxy: true,
        })
    );
}

#[test]
fn pac_script_can_branch_on_host() {
    let target = target();
    let script = r#"
        function FindProxyForURL(url, host) {
            if (dnsDomainIs(host, "api.example"))
                return "PROXY 10.0.0.1:8080";
            return "DIRECT";
        }
    "#;
    assert_eq!(
        evaluate_pac_script(script, &url("https://api.example/v1"), &target),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("10.0.0.1"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        evaluate_pac_script(script, &url("https://other.example/v1"), &target),
        Ok(ResolvedProxy::NoProxy)
    );
}

#[test]
fn pac_script_errors_are_invalid() {
    let target = target();
    let destination = url("https://api.example/v1");
    assert_eq!(
        evaluate_pac_script(
            "function FindProxyForURL(url, host) { throw 1; }",
            &destination,
            &target
        ),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
    assert_eq!(
        evaluate_pac_script(
            "function FindProxyForURL(url, host) { return \"\"; }",
            &destination,
            &target
        ),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
    assert_eq!(
        evaluate_pac_script(
            "function FindProxyForURL(url, host) { return \"SOCKS4 127.0.0.1:1080\"; }",
            &destination,
            &target
        ),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
}

#[test]
fn pac_script_rejects_unbounded_loops() {
    let target = target();
    assert_eq!(
        evaluate_pac_script(
            "function FindProxyForURL(url, host) { while (true) {} }",
            &url("https://api.example/v1"),
            &target,
        ),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
}

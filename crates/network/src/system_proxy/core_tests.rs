use std::{
    sync::{Arc, Mutex, atomic::AtomicU64, mpsc},
    thread,
    time::Instant,
};

use url::Url;

use crate::system_proxy::core::{FingerprintCache, destination_is_local, refresh_fingerprint};

fn url(raw: &str) -> Url {
    Url::parse(raw).unwrap()
}

#[test]
fn implicit_bypass_should_recognize_local_hosts_and_addresses() {
    for local in [
        "http://localhost/",
        "http://localhost:18010/",
        "http://account.localhost/oauth/token",
        "http://api.localhost/",
        "ws://api.localhost/",
        "http://127.0.0.1:18010/",
        "http://127.9.9.9/",
        "http://[::1]:18010/",
        "http://169.254.10.1/",
        "http://[fe80::1]/",
    ] {
        assert!(destination_is_local(&url(local)), "{local}");
    }

    for remote in [
        "https://account.kytrix.xyz/oauth/token",
        "https://api.kytrix.xyz/",
        "https://pbs.twimg.com/media/test.jpg",
        "http://localhost.example/",
        "http://notlocalhost/",
        "http://account.localhost.example/",
        "http://127.0.0.1.example/",
        "http://10.0.0.5/",
        "http://[2001:db8::1]/",
    ] {
        assert!(!destination_is_local(&url(remote)), "{remote}");
    }
}

#[test]
fn fingerprint_discovery_does_not_hold_the_cache_lock() {
    let cache = Mutex::new(None);
    let sequence = AtomicU64::new(1);
    assert_eq!(
        refresh_fingerprint(&cache, &sequence, || {
            assert!(cache.try_lock().is_ok());
            42
        }),
        42
    );
}

#[test]
fn fresh_fingerprint_cache_skips_platform_discovery() {
    let cache = Mutex::new(Some(FingerprintCache::new(17, Instant::now(), 1)));
    let sequence = AtomicU64::new(2);
    assert_eq!(
        refresh_fingerprint(&cache, &sequence, || panic!(
            "fresh cache must not rediscover settings"
        )),
        17
    );
}

#[test]
fn older_fingerprint_discovery_cannot_replace_a_newer_completion() {
    let cache = Arc::new(Mutex::new(None));
    let sequence = Arc::new(AtomicU64::new(1));
    let (older_started, wait_for_older) = mpsc::sync_channel(0);
    let (release_older, older_may_finish) = mpsc::sync_channel(0);

    thread::scope(|scope| {
        let older_cache = Arc::clone(&cache);
        let older_sequence = Arc::clone(&sequence);
        let older = scope.spawn(move || {
            refresh_fingerprint(&older_cache, &older_sequence, || {
                older_started.send(()).unwrap();
                older_may_finish.recv().unwrap();
                17
            })
        });
        wait_for_older.recv().unwrap();

        assert_eq!(refresh_fingerprint(&cache, &sequence, || 29), 29);
        release_older.send(()).unwrap();
        assert_eq!(older.join().unwrap(), 29);
    });
    assert_eq!(
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(FingerprintCache::value),
        Some(29)
    );
}

#[test]
fn newer_fingerprint_discovery_publishes_after_an_older_completion() {
    let cache = Arc::new(Mutex::new(None));
    let sequence = Arc::new(AtomicU64::new(1));
    let (older_started, wait_for_older) = mpsc::sync_channel(0);
    let (newer_started, wait_for_newer) = mpsc::sync_channel(0);
    let (release_older, older_may_finish) = mpsc::sync_channel(0);
    let (release_newer, newer_may_finish) = mpsc::sync_channel(0);

    thread::scope(|scope| {
        let older_cache = Arc::clone(&cache);
        let older_sequence = Arc::clone(&sequence);
        let older = scope.spawn(move || {
            refresh_fingerprint(&older_cache, &older_sequence, || {
                older_started.send(()).unwrap();
                older_may_finish.recv().unwrap();
                17
            })
        });
        wait_for_older.recv().unwrap();

        let newer_cache = Arc::clone(&cache);
        let newer_sequence = Arc::clone(&sequence);
        let newer = scope.spawn(move || {
            refresh_fingerprint(&newer_cache, &newer_sequence, || {
                newer_started.send(()).unwrap();
                newer_may_finish.recv().unwrap();
                29
            })
        });
        wait_for_newer.recv().unwrap();

        release_older.send(()).unwrap();
        assert_eq!(older.join().unwrap(), 17);
        release_newer.send(()).unwrap();
        assert_eq!(newer.join().unwrap(), 29);
    });
    assert_eq!(
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(FingerprintCache::value),
        Some(29)
    );
}

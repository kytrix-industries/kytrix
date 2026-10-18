use std::{
    collections::BTreeMap,
    process::Command,
    time::{Duration, Instant},
};

use app_config::network::{ConfigError, KytrixServiceCapability, ResolvedProxy, ServiceTarget};
use url::Url;

use crate::{
    error::NetworkError,
    system_proxy::linux::{
        command::bounded_command_output_with_timeout,
        gnome::{
            gnome_autoconfig_url_from_value, gnome_endpoint_from_values, gnome_from_get,
            gnome_manual_config, gnome_use_same_from_value,
        },
        kde::{kde_config_from_keys, kde_config_from_kioslaverc, merge_kde_keys},
        settings::{
            LinuxDesktop, LinuxProxyConfig, LinuxSettings, config_from_values, evaluate_settings,
            fingerprint_settings, linux_config_from_env, linux_desktop_from_env,
        },
        wpad::{network_manager_wpad_command, network_manager_wpad_url_from_output},
    },
};

fn target() -> RouteTarget {
    RouteTarget::Service(ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Account,
    })
}

fn env<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |key| {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_owned())
    }
}

#[test]
fn discovery_command_collects_bounded_output() {
    let mut command = Command::new("rustc");
    command.arg("--version");
    let output = bounded_command_output_with_timeout(&mut command, Duration::from_secs(2))
        .expect("rustc should finish within the discovery deadline");
    assert!(output.status.success());
    assert!(!output.stdout.is_empty());
}

#[test]
fn network_manager_wpad_disables_terse_escaping_and_parses_option_252() {
    let command = network_manager_wpad_command();
    assert_eq!(command.get_program(), "nmcli");
    assert_eq!(
        command
            .get_args()
            .map(|argument| argument.to_str().unwrap())
            .collect::<Vec<_>>(),
        ["-t", "-e", "no", "-f", "DHCP4.OPTION", "device", "show"]
    );
    assert_eq!(
        network_manager_wpad_url_from_output(
            "DHCP4.OPTION[1]:dhcp_server_identifier = 192.0.2.1\n\
             DHCP4.OPTION[2]:wpad = http://wpad.corp.example/proxy.pac\n"
        ),
        Some("http://wpad.corp.example/proxy.pac".into())
    );
}

#[cfg(unix)]
#[test]
fn discovery_command_is_killed_at_its_deadline() {
    let mut command = Command::new("sh");
    command.args(["-c", "while :; do :; done"]);
    let started = Instant::now();
    assert!(bounded_command_output_with_timeout(&mut command, Duration::from_millis(50)).is_none());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn linux_desktop_follows_chromium_xdg_current_desktop() {
    assert_eq!(
        linux_desktop_from_env(env(&[("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")])),
        LinuxDesktop::GnomeFamily
    );
    assert_eq!(
        linux_desktop_from_env(env(&[("XDG_CURRENT_DESKTOP", "KDE")])),
        LinuxDesktop::Kde
    );
    assert_eq!(
        linux_desktop_from_env(env(&[("XDG_CURRENT_DESKTOP", "XFCE")])),
        LinuxDesktop::Other
    );
}

#[test]
fn linux_env_matches_chromium_https_and_does_not_reuse_http_proxy() {
    assert_eq!(
        linux_config_from_env(env(&[("https_proxy", "http://proxy.example:8080")])),
        Some(LinuxProxyConfig::Manual {
            http: None,
            https: Some(("proxy.example".into(), 8080)),
            https_socks: None,
            socks: None,
            use_same: false,
        })
    );
    assert_eq!(
        linux_config_from_env(env(&[("http_proxy", "http://proxy.example:8080")])),
        Some(LinuxProxyConfig::Manual {
            http: Some(("proxy.example".into(), 8080)),
            https: None,
            https_socks: None,
            socks: None,
            use_same: false,
        })
    );
    assert_eq!(
        linux_config_from_env(env(&[("all_proxy", "socks5://127.0.0.1:7891")])),
        Some(LinuxProxyConfig::Manual {
            http: None,
            https: None,
            https_socks: None,
            socks: Some(("127.0.0.1".into(), 7891)),
            use_same: false,
        })
    );
    assert_eq!(
        linux_config_from_env(env(&[("all_proxy", "socks5h://127.0.0.1:7891")])),
        Some(LinuxProxyConfig::Manual {
            http: None,
            https: None,
            https_socks: None,
            socks: Some(("127.0.0.1".into(), 7891)),
            use_same: false,
        })
    );
    assert_eq!(
        linux_config_from_env(env(&[("auto_proxy", "")])),
        Some(LinuxProxyConfig::Direct)
    );
    assert_eq!(
        linux_config_from_env(env(&[("auto_proxy", "http://wpad/proxy.pac")])),
        Some(LinuxProxyConfig::Pac {
            url: Some("http://wpad/proxy.pac".into()),
        })
    );
}

#[test]
fn https_socks_environment_proxy_is_not_used_for_http() {
    let config = linux_config_from_env(env(&[("https_proxy", "socks5://127.0.0.1:7891")]))
        .expect("https proxy should be parsed");
    let target = target();
    assert_eq!(
        evaluate_settings(
            Some(LinuxSettings {
                config,
                bypass: Vec::new(),
            }),
            None,
            &Url::parse("http://api.example/v1").unwrap(),
            0,
            &target,
        ),
        Ok(ResolvedProxy::NoProxy)
    );
}

#[test]
fn linux_never_answers_a_local_destination_with_a_proxy() {
    let target = target();
    let local = Url::parse("http://account.localhost/oauth/token").unwrap();

    for config in [
        LinuxProxyConfig::Manual {
            http: Some(("proxy.example".into(), 8080)),
            https: None,
            https_socks: None,
            socks: None,
            use_same: true,
        },
        LinuxProxyConfig::Pac {
            url: Some("http://wpad/proxy.pac".into()),
        },
        LinuxProxyConfig::Invalid,
    ] {
        assert_eq!(
            evaluate_settings(
                Some(LinuxSettings {
                    config,
                    bypass: Vec::new(),
                }),
                None,
                &local,
                0,
                &target,
            ),
            Ok(ResolvedProxy::NoProxy),
            "{local}"
        );
    }
}

#[test]
fn linux_http_proxy_accepts_bracketed_ipv6() {
    assert_eq!(
        linux_config_from_env(env(&[("http_proxy", "http://[::1]:8080")])),
        Some(LinuxProxyConfig::Manual {
            http: Some(("[::1]".into(), 8080)),
            https: None,
            https_socks: None,
            socks: None,
            use_same: false,
        })
    );
}

#[test]
fn malformed_all_proxy_is_invalid_and_does_not_fall_through() {
    assert_eq!(
        linux_config_from_env(env(&[
            ("all_proxy", "http://proxy:notaport"),
            ("https_proxy", "http://127.0.0.1:8080"),
        ])),
        Some(LinuxProxyConfig::Invalid)
    );
}

#[test]
fn non_ascii_all_proxy_does_not_panic() {
    let parsed = linux_config_from_env(env(&[("all_proxy", "你好你好")]));
    assert!(parsed.is_some());
}

#[test]
fn linux_socks_proxy_accepts_host_names_and_bracketed_ipv6() {
    let target = target();
    for (value, host) in [
        ("socks5://proxy.example:7890", "proxy.example"),
        ("socks5h://[2001:db8::1]:7890", "[2001:db8::1]"),
    ] {
        let config = linux_config_from_env(env(&[("all_proxy", value)]))
            .unwrap_or_else(|| panic!("{value} should be parsed"));
        assert_eq!(
            evaluate_settings(
                Some(LinuxSettings {
                    config,
                    bypass: Vec::new(),
                }),
                None,
                &Url::parse("https://api.example/v1").unwrap(),
                0,
                &target,
            ),
            Ok(ResolvedProxy::Socks5 {
                host: smol_str::SmolStr::new(host),
                port: 7890,
                credentials: None,
                resolve_dns_at_remote: true,
            }),
            "{value}"
        );
    }
}

#[test]
fn linux_manual_selects_by_destination_scheme() {
    let target = target();
    let config = LinuxProxyConfig::Manual {
        http: Some(("http.proxy".into(), 8080)),
        https: Some(("https.proxy".into(), 8443)),
        https_socks: None,
        socks: None,
        use_same: false,
    };
    assert_eq!(
        evaluate_settings(
            Some(LinuxSettings {
                config: config.clone(),
                bypass: Vec::new(),
            }),
            None,
            &Url::parse("https://api.example/v1").unwrap(),
            0,
            &target,
        ),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("https.proxy"),
            port: 8443,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        evaluate_settings(
            Some(LinuxSettings {
                config,
                bypass: Vec::new(),
            }),
            None,
            &Url::parse("http://api.example/v1").unwrap(),
            0,
            &target,
        ),
        Ok(ResolvedProxy::Http {
            host: smol_str::SmolStr::new_static("http.proxy"),
            port: 8080,
            credentials: None,
            tls_to_proxy: false,
        })
    );
    assert_eq!(
        evaluate_settings(
            Some(LinuxSettings {
                config: LinuxProxyConfig::Manual {
                    http: Some(("http.proxy".into(), 8080)),
                    https: None,
                    https_socks: None,
                    socks: None,
                    use_same: false,
                },
                bypass: Vec::new(),
            }),
            None,
            &Url::parse("https://api.example/v1").unwrap(),
            0,
            &target,
        ),
        Ok(ResolvedProxy::NoProxy)
    );
}

#[test]
fn gnome_manual_matches_chromium_same_proxy_and_https_fallback() {
    assert_eq!(
        config_from_values("none", false, "", None, None, None),
        Some(LinuxProxyConfig::Direct)
    );
    assert_eq!(
        config_from_values("auto", false, "http://wpad/proxy.pac", None, None, None),
        Some(LinuxProxyConfig::Pac {
            url: Some("http://wpad/proxy.pac".into()),
        })
    );
    assert_eq!(
        config_from_values("auto", false, "", None, None, None),
        Some(LinuxProxyConfig::Pac { url: None })
    );
    assert_eq!(
        config_from_values(
            "manual",
            false,
            "",
            Some(("127.0.0.1".into(), 7890)),
            None,
            None
        ),
        Some(LinuxProxyConfig::Manual {
            http: Some(("127.0.0.1".into(), 7890)),
            https: None,
            https_socks: None,
            socks: None,
            use_same: false,
        })
    );
}

#[test]
fn gnome_endpoint_distinguishes_absent_from_invalid_values() {
    assert_eq!(
        gnome_endpoint_from_values(Some("''".into()), None, false),
        Ok(None)
    );
    for (host, port) in [
        (None, Some("8080".into())),
        (Some("'proxy.example'".into()), None),
        (Some("'proxy.example'".into()), Some("uint32 0".into())),
        (
            Some("'proxy.example'".into()),
            Some("uint32 invalid".into()),
        ),
    ] {
        assert!(
            matches!(
                gnome_endpoint_from_values(host, port, false),
                Err(NetworkError::InvalidData { .. })
            ),
            "the endpoint must be invalid"
        );
    }
}

#[test]
fn gnome_manual_config_rejects_one_invalid_endpoint() {
    assert_eq!(
        gnome_manual_config(
            false,
            Ok(Some(("http.proxy".into(), 8080))),
            Err(NetworkError::InvalidData {
                operation: "read GNOME proxy endpoint".into(),
                reason: "proxy port is invalid".into(),
            }),
            Ok(None),
        ),
        LinuxProxyConfig::Invalid
    );
}

#[test]
fn gnome_manual_use_same_read_failure_is_invalid() {
    let settings = gnome_from_get(|schema, key| match (schema, key) {
        ("org.gnome.system.proxy", "mode") => Some("'manual'".into()),
        ("org.gnome.system.proxy", "use-same-proxy") => None,
        _ => panic!("manual parsing must stop after use-same-proxy fails"),
    })
    .expect("the desktop settings were readable");

    assert_eq!(settings.config, LinuxProxyConfig::Invalid);
    assert!(matches!(
        gnome_use_same_from_value(Some("'invalid'".into())),
        Err(NetworkError::InvalidData { .. })
    ));
}

#[test]
fn gnome_auto_autoconfig_read_failure_is_invalid() {
    let settings = gnome_from_get(|schema, key| match (schema, key) {
        ("org.gnome.system.proxy", "mode") => Some("'auto'".into()),
        ("org.gnome.system.proxy", "autoconfig-url") => None,
        _ => panic!("auto parsing must stop after autoconfig-url fails"),
    })
    .expect("the desktop mode was readable");

    assert_eq!(settings.config, LinuxProxyConfig::Invalid);
}

#[test]
fn gnome_auto_readable_empty_autoconfig_uses_wpad() {
    let settings = gnome_from_get(|schema, key| match (schema, key) {
        ("org.gnome.system.proxy", "mode") => Some("'auto'".into()),
        ("org.gnome.system.proxy", "autoconfig-url") => Some("''".into()),
        ("org.gnome.system.proxy", "ignore-hosts") => Some("[]".into()),
        _ => None,
    })
    .expect("the desktop settings were readable");

    assert_eq!(settings.config, LinuxProxyConfig::Pac { url: None });
}

#[test]
fn gnome_auto_rejects_malformed_autoconfig_output() {
    for raw in [
        "http://wpad/proxy.pac",
        "'not a URL'",
        "'file:///proxy.pac'",
    ] {
        assert!(
            matches!(
                gnome_autoconfig_url_from_value(Some(raw.into())),
                Err(NetworkError::InvalidData { .. })
            ),
            "{raw} must be rejected"
        );
    }
    assert_eq!(
        gnome_autoconfig_url_from_value(Some("'https://wpad/proxy.pac'".into())),
        Ok("https://wpad/proxy.pac".into())
    );
}

#[test]
fn gnome_none_reads_only_the_mode() {
    let reads = std::cell::Cell::new(0);
    let settings = gnome_from_get(|schema, key| {
        reads.set(reads.get() + 1);
        assert_eq!((schema, key), ("org.gnome.system.proxy", "mode"));
        Some("'none'".into())
    })
    .expect("the desktop settings were readable");

    assert_eq!(settings.config, LinuxProxyConfig::Direct);
    assert_eq!(reads.get(), 1);
}

#[test]
fn kde_merges_keys_across_files() {
    let mut keys = BTreeMap::new();
    merge_kde_keys(
        &mut keys,
        "[Proxy Settings]\nProxyType=1\nhttpsProxy=127.0.0.1 8080\nhttpProxy=10.0.0.1 80\n",
    );
    merge_kde_keys(
        &mut keys,
        "[Proxy Settings]\nProxyType=1\nhttpProxy=127.0.0.1 7890\n",
    );
    assert_eq!(
        kde_config_from_keys(&keys, env(&[])),
        Some(LinuxProxyConfig::Manual {
            http: Some(("127.0.0.1".into(), 7890)),
            https: Some(("127.0.0.1".into(), 8080)),
            https_socks: None,
            socks: None,
            use_same: false,
        })
    );
}

#[test]
fn kde_reads_separate_port_keys() {
    assert_eq!(
        kde_config_from_kioslaverc(
            "[Proxy Settings]\nProxyType=1\nhttpsProxy=127.0.0.1\nhttpsPort=8080\n",
            env(&[])
        ),
        Some(LinuxProxyConfig::Manual {
            http: None,
            https: Some(("127.0.0.1".into(), 8080)),
            https_socks: None,
            socks: None,
            use_same: false,
        })
    );
}

#[test]
fn kde_manual_proxy_rejects_any_configured_malformed_endpoint_or_port() {
    assert_eq!(
        kde_config_from_kioslaverc(
            "[Proxy Settings]\nProxyType=1\nhttpProxy=127.0.0.1 8080\nhttpsProxy=[::1\n",
            env(&[]),
        ),
        Some(LinuxProxyConfig::Invalid)
    );
    assert_eq!(
        kde_config_from_kioslaverc(
            "[Proxy Settings]\nProxyType=1\nhttpProxy=127.0.0.1 8080\nhttpsProxy=proxy.example\nhttpsPort=notaport\n",
            env(&[]),
        ),
        Some(LinuxProxyConfig::Invalid)
    );
}

#[test]
fn kde_environment_proxy_rejects_any_configured_malformed_endpoint_or_port() {
    assert_eq!(
        kde_config_from_kioslaverc(
            "[Proxy Settings]\nProxyType=4\nhttpProxy=HTTP_PROXY\nhttpsProxy=HTTPS_PROXY\n",
            env(&[("HTTP_PROXY", "127.0.0.1:8080"), ("HTTPS_PROXY", "[::1"),]),
        ),
        Some(LinuxProxyConfig::Invalid)
    );
    assert_eq!(
        kde_config_from_kioslaverc(
            "[Proxy Settings]\nProxyType=4\nhttpProxy=HTTP_PROXY\nhttpsProxy=HTTPS_PROXY\nhttpsPort=bad\n",
            env(&[
                ("HTTP_PROXY", "127.0.0.1:8080"),
                ("HTTPS_PROXY", "proxy.example"),
            ]),
        ),
        Some(LinuxProxyConfig::Invalid)
    );
}

#[test]
fn kde_kioslaverc_matches_chromium_proxy_types() {
    assert_eq!(
        kde_config_from_kioslaverc(
            "[Proxy Settings]\nProxyType=1\nhttpsProxy=127.0.0.1 8080\n",
            env(&[])
        ),
        Some(LinuxProxyConfig::Manual {
            http: None,
            https: Some(("127.0.0.1".into(), 8080)),
            https_socks: None,
            socks: None,
            use_same: false,
        })
    );
    assert_eq!(
        kde_config_from_kioslaverc("[Proxy Settings]\nProxyType=2\n", env(&[])),
        Some(LinuxProxyConfig::Pac { url: None })
    );
    assert_eq!(
        kde_config_from_kioslaverc("[Proxy Settings]\nProxyType=3\n", env(&[])),
        Some(LinuxProxyConfig::Pac { url: None })
    );
    assert_eq!(
        kde_config_from_kioslaverc("[Proxy Settings]\nProxyType=bogus\n", env(&[])),
        Some(LinuxProxyConfig::Invalid)
    );
    assert_eq!(
        kde_config_from_kioslaverc("[Proxy Settings]\n", env(&[])),
        Some(LinuxProxyConfig::Invalid)
    );
}

#[test]
fn auto_detect_fingerprint_includes_the_discovered_wpad_url() {
    let settings = Some(LinuxSettings {
        config: LinuxProxyConfig::Pac { url: None },
        bypass: Vec::new(),
    });

    assert_ne!(
        fingerprint_settings(&settings, Some("http://wpad.first/wpad.dat")),
        fingerprint_settings(&settings, Some("http://wpad.second/wpad.dat"))
    );
    assert_ne!(
        fingerprint_settings(&settings, Some("http://wpad.first/wpad.dat")),
        fingerprint_settings(&settings, None)
    );
}

#[test]
fn linux_settings_debug_stays_redaction_free_and_structured() {
    let settings = LinuxSettings {
        config: LinuxProxyConfig::Direct,
        bypass: vec!["<local>".into()],
    };
    assert_eq!(
        format!("{settings:?}"),
        "LinuxSettings { config: Direct, bypass: [\"<local>\"] }"
    );
}

#[test]
fn invalid_linux_settings_fail_closed() {
    let target = target();
    assert_eq!(
        evaluate_settings(
            Some(LinuxSettings {
                config: LinuxProxyConfig::Invalid,
                bypass: Vec::new(),
            }),
            None,
            &Url::parse("https://api.example/v1").unwrap(),
            0,
            &target,
        ),
        Err(ConfigError::InvalidSystemProxy {
            target: target.clone()
        })
    );
}

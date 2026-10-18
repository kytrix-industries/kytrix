use url::Url;

use crate::{
    error::NetworkError,
    system_proxy::bypass::{destination_is_bypassed, glob_match},
};

#[test]
fn wildcard_matching_is_iterative_and_bounded() {
    assert_eq!(glob_match("api.corp.example", "*.corp.example"), Ok(true));
    assert_eq!(glob_match("api1.example", "api?.example"), Ok(true));
    assert_eq!(glob_match("api12.example", "api?.example"), Ok(false));

    let value = "a".repeat(2_000);
    let pattern = format!("{}b", "*a".repeat(2_000));
    assert!(matches!(
        glob_match(&value, &pattern),
        Err(NetworkError::LimitExceeded { .. })
    ));
}

fn url(raw: &str) -> Url {
    Url::parse(raw).unwrap()
}

#[test]
fn bypass_rules_match_chromium_shapes() {
    let destination = url("https://intranet.corp.example/api");
    assert!(destination_is_bypassed(
        &destination,
        &["*.corp.example".into()]
    ));
    assert!(destination_is_bypassed(
        &url("https://localhost/"),
        &["<local>".into()]
    ));
    assert!(destination_is_bypassed(
        &url("https://10.0.0.5/"),
        &["10.0.0.0/8".into()]
    ));
    assert!(!destination_is_bypassed(
        &url("https://api.example/"),
        &["*.corp.example".into()]
    ));
    assert!(destination_is_bypassed(
        &url("https://api.example:8443/"),
        &["api.example:8443".into()]
    ));
    assert!(!destination_is_bypassed(
        &url("https://api.example:443/"),
        &["api.example:8443".into()]
    ));
    assert!(destination_is_bypassed(
        &url("https://10.0.0.5:9443/"),
        &["10.0.0.5:9443".into()]
    ));
    assert!(destination_is_bypassed(
        &url("https://[::1]:9443/"),
        &["[::1]:9443".into()]
    ));
    assert!(destination_is_bypassed(
        &url("https://api.example/"),
        &["api.example:443".into()]
    ));
    assert!(destination_is_bypassed(
        &url("https://[2001:db8::1]/"),
        &["2001:db8::/32".into()]
    ));
}

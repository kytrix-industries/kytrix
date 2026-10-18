use app_config::network::ResolvedProxy;
use reqwest::header::HeaderMap;
use url::Url;

use crate::transport::{
    error::TransportError,
    hub::{TransportHub, TransportPolicy},
};

fn hub(policy: TransportPolicy) -> TransportHub {
    TransportHub::new(policy)
}

fn plaintext_hub() -> TransportHub {
    hub(TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    })
}

#[test]
fn a_strict_policy_denies_plaintext_websockets() {
    let hub = hub(TransportPolicy::default());
    assert!(!hub.policy().allow_plaintext);
}

#[tokio::test]
async fn a_plaintext_websocket_endpoint_is_rejected_by_a_strict_hub() {
    let hub = hub(TransportPolicy::default());
    let url = Url::parse("ws://127.0.0.1:9000/ws").unwrap();

    let result = hub
        .connect_websocket(
            &url,
            &ResolvedProxy::NoProxy,
            HeaderMap::new(),
            &hub.cancellation().child_token(),
        )
        .await;

    assert!(matches!(
        result,
        Err(TransportError::InvalidWebSocketEndpoint)
    ));
}

#[tokio::test]
async fn a_non_websocket_scheme_is_rejected() {
    let hub = plaintext_hub();
    let url = Url::parse("https://127.0.0.1:9000/ws").unwrap();

    let result = hub
        .connect_websocket(
            &url,
            &ResolvedProxy::NoProxy,
            HeaderMap::new(),
            &hub.cancellation().child_token(),
        )
        .await;

    assert!(matches!(
        result,
        Err(TransportError::InvalidWebSocketEndpoint)
    ));
}

#[tokio::test]
async fn a_cancelled_hub_refuses_new_connections() {
    let hub = plaintext_hub();
    let cancellation = hub.cancellation().clone();
    cancellation.cancel();
    let url = Url::parse("ws://127.0.0.1:9000/ws").unwrap();

    let result = hub
        .connect_websocket(
            &url,
            &ResolvedProxy::NoProxy,
            HeaderMap::new(),
            &cancellation,
        )
        .await;

    assert!(matches!(result, Err(TransportError::Cancelled)));
}

#[tokio::test]
async fn a_scope_cancels_only_its_own_operations() {
    let hub = plaintext_hub();
    let scope = hub.scope();
    let permit = scope.begin_operation().unwrap();
    let global = hub.begin_operation().unwrap();

    scope.cancel();
    assert!(scope.cancellation().is_cancelled());
    assert!(!hub.cancellation().is_cancelled());
    assert!(scope.begin_operation().is_err());
    assert!(hub.begin_operation().is_ok());

    drop((permit, global));
}

#[tokio::test]
async fn a_shutdown_waits_for_operations_and_closes_the_hub() {
    let hub = plaintext_hub();
    let permit = hub.begin_operation().unwrap();
    let shutdown = {
        let hub = hub.clone();
        tokio::spawn(async move { hub.shutdown().await })
    };
    tokio::task::yield_now().await;

    assert!(!shutdown.is_finished(), "shutdown must wait for the permit");
    assert!(hub.begin_operation().is_err());

    drop(permit);
    tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
        .await
        .expect("shutdown should finish once the permit is dropped")
        .unwrap();
}

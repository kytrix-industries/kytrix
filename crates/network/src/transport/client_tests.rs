use std::{net::Ipv4Addr, process::Command, time::Duration};

use app_config::network::{ProxyCredentials, ResolvedProxy};
use reqwest::StatusCode;
use smol_str::SmolStr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    time::timeout,
};

use crate::{
    test_support::{Socks5Proxy, Socks5Target, spawn_static_http_server},
    transport::client::{self, ClientSpec},
};

fn spec() -> ClientSpec {
    ClientSpec {
        connect_timeout: Duration::from_secs(5),
        request_timeout: Some(Duration::from_secs(2)),
        user_agent: Some("Kytrix/test"),
        default_headers: None,
        tcp_nodelay: false,
        http1_only: false,
    }
}

fn socks5(port: u16, credentials: Option<ProxyCredentials>, remote_dns: bool) -> ResolvedProxy {
    ResolvedProxy::Socks5 {
        host: SmolStr::new_static("127.0.0.1"),
        port,
        credentials,
        resolve_dns_at_remote: remote_dns,
    }
}

#[test]
fn every_route_shape_builds_a_client() {
    let routes = [
        ResolvedProxy::NoProxy,
        socks5(1080, None, false),
        socks5(
            1080,
            Some(ProxyCredentials {
                username: SmolStr::new_static("user"),
                password: SmolStr::new_static("password"),
            }),
            true,
        ),
        ResolvedProxy::Http {
            host: SmolStr::new_static("127.0.0.1"),
            port: 7890,
            credentials: None,
            tls_to_proxy: false,
        },
        ResolvedProxy::Http {
            host: SmolStr::new_static("127.0.0.1"),
            port: 8443,
            credentials: Some(ProxyCredentials {
                username: SmolStr::new_static("user"),
                password: SmolStr::new_static("password"),
            }),
            tls_to_proxy: true,
        },
    ];

    for route in routes {
        assert!(
            client::build(&route, spec(), false, &[]).is_ok(),
            "{route:?}"
        );
    }
}

#[tokio::test]
async fn a_strict_client_refuses_a_plaintext_request() {
    let client = client::build(&ResolvedProxy::NoProxy, spec(), true, &[]).unwrap();

    let error = client
        .get("http://127.0.0.1:1/plaintext")
        .send()
        .await
        .unwrap_err();

    assert!(error.is_builder());
}

#[tokio::test]
async fn a_direct_client_reaches_a_plaintext_origin_without_a_proxy() {
    let (address, server) = spawn_static_http_server("direct-body").await;
    let client = client::build(&ResolvedProxy::NoProxy, spec(), false, &[]).unwrap();

    let response = client
        .get(format!("http://{address}/direct"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "direct-body");
    server.await.unwrap();
}

#[tokio::test]
async fn a_local_socks5_route_resolves_the_target_before_connecting() {
    let (media_address, media_task) = spawn_static_http_server("media-through-proxy").await;
    let mut proxy = Socks5Proxy::start(None).await;
    let client = client::build(
        &socks5(proxy.address.port(), None, false),
        spec(),
        false,
        &[],
    )
    .unwrap();

    let body = client
        .get(format!("http://{media_address}/media"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(body, "media-through-proxy");
    assert_eq!(
        proxy.observed_target().await,
        Socks5Target::Addr(media_address)
    );
    media_task.await.unwrap();
}

#[tokio::test]
async fn an_authenticated_socks5h_route_leaves_the_host_name_to_the_proxy() {
    let (media_address, media_task) = spawn_static_http_server("media-through-proxy").await;
    let mut proxy =
        Socks5Proxy::start_resolving_domains(Some(("media user", "p@ss:word")), media_address)
            .await;
    let route = socks5(
        proxy.address.port(),
        Some(ProxyCredentials {
            username: SmolStr::new_static("media user"),
            password: SmolStr::new_static("p@ss:word"),
        }),
        true,
    );
    let client = client::build(&route, spec(), false, &[]).unwrap();

    let body = client
        .get(format!("http://media.test:{}/media", media_address.port()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(body, "media-through-proxy");
    assert_eq!(
        proxy.observed_target().await,
        Socks5Target::Domain {
            host: "media.test".into(),
            port: media_address.port(),
        }
    );
    media_task.await.unwrap();
}

#[tokio::test]
async fn an_unreachable_socks_route_fails_closed_without_a_direct_fallback() {
    let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let destination_address = destination.local_addr().unwrap();
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move {
        let (stream, _) = proxy.accept().await.unwrap();
        drop(stream);
    });
    let client = client::build(
        &socks5(proxy_address.port(), None, true),
        spec(),
        false,
        &[],
    )
    .unwrap();

    let error = client
        .get(format!("http://{destination_address}/must-not-connect"))
        .send()
        .await
        .unwrap_err();

    assert!(!error.is_timeout());
    assert!(
        timeout(Duration::from_millis(50), destination.accept())
            .await
            .is_err()
    );
    proxy_task.await.unwrap();
}

#[tokio::test]
async fn a_client_honors_its_request_timeout_and_user_agent() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0_u8; 256];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        }
        let _ = request_sender.send(String::from_utf8_lossy(&request).into_owned());
        std::future::pending::<()>().await;
    });
    let mut spec = spec();
    spec.request_timeout = Some(Duration::from_millis(50));
    let client = client::build(&ResolvedProxy::NoProxy, spec, false, &[]).unwrap();

    let error = client
        .get(format!("http://{address}/slow"))
        .send()
        .await
        .unwrap_err();
    let request = request_receiver.await.unwrap();

    assert!(error.is_timeout());
    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("user-agent: Kytrix/test")),
        "{request}"
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn zero_redirect_clients_never_contact_the_redirect_target() {
    let source = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let source_address = source.local_addr().unwrap();
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target_address = target.local_addr().unwrap();
    let source_task = tokio::spawn(async move {
        let (mut stream, _) = source.accept().await.unwrap();
        let mut request = [0_u8; 1_024];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let client = client::build(&ResolvedProxy::NoProxy, spec(), false, &[]).unwrap();

    let response = client
        .get(format!("http://{source_address}/redirect"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
    assert!(
        timeout(Duration::from_millis(100), target.accept())
            .await
            .is_err(),
        "a redirect was followed"
    );
    source_task.await.unwrap();
}

#[tokio::test]
async fn a_client_ignores_environment_proxies() {
    const CHILD_MARKER: &str = "KYTRIX_HTTP_PROXY_TEST_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let (address, server) = spawn_static_http_server("ok").await;
        let client = client::build(&ResolvedProxy::NoProxy, spec(), false, &[]).unwrap();
        let body = client
            .get(format!("http://{address}/direct"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
        server.await.unwrap();
        return;
    }

    let executable = std::env::current_exe().unwrap();
    let output = Command::new(executable)
        .args([
            "--exact",
            "transport::tests::client::a_client_ignores_environment_proxies",
            "--nocapture",
        ])
        .env(CHILD_MARKER, "1")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child proxy test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

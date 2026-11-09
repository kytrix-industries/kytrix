//! The update metadata end to end, against a server that answers the way GitHub does.
//!
//! The request goes through a real transport, a real client and a real redirect
//! chain, so what is exercised here is the path production takes: the endpoint behind
//! a redirect, and the release it publishes behind another one.

use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use app_config::network::Config;
use bytes::Bytes;
use hyper::{
    Request, Response, StatusCode,
    body::{Body, Frame, Incoming},
    header::LOCATION,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use network::transport::hub::{TransportHub, TransportPolicy};
use serde_json::json;
use tokio::net::TcpListener;
use updater_ui::{
    fetch,
    release::{PlatformKey, ReleaseFormat},
};
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

/// The response body the release server answers with: its whole entity in one frame.
struct TestBody(Option<Bytes>);

impl TestBody {
    /// A body that carries `bytes`.
    fn whole(bytes: Bytes) -> Self {
        Self(Some(bytes))
    }

    /// A body that carries nothing, which is what a redirect has.
    fn empty() -> Self {
        Self(None)
    }
}

impl Body for TestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.get_mut().0.take().map(|bytes| Ok(Frame::data(bytes))))
    }
}

fn answer(status: StatusCode, body: TestBody, location: Option<&str>) -> Response<TestBody> {
    let mut answer = Response::builder().status(status);
    if let Some(location) = location {
        answer = answer.header(LOCATION, location);
    }

    answer.body(body).expect("the response is well formed")
}

/// What the endpoint publishes: one release, naming the payload at `payload_path`.
fn metadata(address: SocketAddr, payload_path: &str) -> Bytes {
    let key = PlatformKey::current().expect("the tests run on a packaged platform");
    let published = json!({
        "version": "9.9.9",
        "notes": {
            "en": "## What changed\n\n- the whole thing",
            "zh-CN": "## 更新内容\n\n- 全部",
        },
        "pub_date": "2026-10-20T09:00:00Z",
        "platforms": {
            key.wire(): {
                "url": format!("http://{address}{payload_path}"),
                "format": "nsis",
            },
        },
    });

    Bytes::from(serde_json::to_vec(&published).expect("the manifest is JSON"))
}

/// Starts a release server: the metadata behind a redirect to a relative location,
/// which is what GitHub sends and what the client has to resolve against the request
/// it made.
async fn spawn_release_server() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("a loopback port is free");
    let address = listener.local_addr().expect("the listener has an address");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let service = service_fn(move |request: Request<Incoming>| async move {
                let response = match request.uri().path() {
                    "/latest.json" => answer(
                        StatusCode::FOUND,
                        TestBody::empty(),
                        Some("/releases/latest.json"),
                    ),
                    "/releases/latest.json" => answer(
                        StatusCode::OK,
                        TestBody::whole(metadata(address, "/payload")),
                        None,
                    ),
                    _ => answer(
                        StatusCode::NOT_FOUND,
                        TestBody::whole(Bytes::from_static(b"not found")),
                        None,
                    ),
                };

                Ok::<_, Infallible>(response)
            });
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    address
}

/// The transport an update request goes through, over loopback.
fn transport() -> TransportHub {
    TransportHub::new(TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    })
}

/// The endpoint the fake release is published at.
fn endpoint(address: SocketAddr) -> Url {
    Url::parse(&format!("http://{address}/latest.json")).expect("the endpoint is a URL")
}

#[tokio::test]
async fn a_release_should_be_read_through_its_redirect_chain() {
    let address = spawn_release_server().await;
    let hub = transport();
    let runtime = Runtime::for_test();
    let config = Config::default();
    let cancellation = hub.cancellation().child_token();

    let release = fetch::fetch_release(&hub, &config, &runtime, &endpoint(address), &cancellation)
        .await
        .expect("the metadata is read through its redirect")
        .expect("a newer release is offered");

    assert_eq!(release.version, semver::Version::new(9, 9, 9));
    assert_eq!(release.asset.format, ReleaseFormat::Nsis);
    assert_eq!(
        release.asset.url.as_str(),
        format!("http://{address}/payload"),
        "the payload is what the metadata named, not what was asked for"
    );
}

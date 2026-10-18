//! The update path end to end, against a server that answers the way GitHub does.
//!
//! The requests go through a real transport, a real client and a real redirect
//! chain, so what is exercised here is the path production takes: metadata behind
//! a redirect, a payload behind another one, frames that arrive one at a time,
//! and a signature that is checked against the bytes that arrived.

use std::{
    collections::VecDeque,
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use app_config::network::Config;
use bytes::Bytes;
use hyper::{
    Request, Response, StatusCode,
    body::{Body, Frame, Incoming, SizeHint},
    header::LOCATION,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use network::transport::{
    error::TransportError,
    hub::{TransportHub, TransportPolicy},
};
use serde_json::json;
use tokio::{net::TcpListener, time::Sleep};
use updater_ui::{
    error::{UpdateRequestError, UpdaterError},
    fetch,
    release::{PlatformKey, ReleaseFormat},
    verify,
};
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

/// How much of the payload each frame carries.
const FRAME: usize = 16 * 1024;

/// How long the payload is: it has to be several frames, so a download in flight
/// is a download that is still going.
const LENGTH: usize = 192 * 1024;

/// A minisign public key made for these tests, in the same form as the published one.
///
/// Generated as documented on `verify.rs`'s own fixture; the private half was
/// discarded after the two payloads below were signed with it.
const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEZBRkIxRDAxODQ4MjdEQzYKUlVUNit4MEJoSUo5eHNCMG5ncHhUdFNnVExmcUdENHNENlArNHZ6MHpnYzJQeHEyU2JPT0F1V1AK";

/// The signature of the payload the server serves.
const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIG1pbmlzaWduIHNlY3JldCBrZXkKUlVUNit4MEJoSUo5eGgvM1A5d2ZUcUUxQjFRT3R0M2I5dGI0cCthZk9sSXFUclpWTzRJNEE2N2lXeldxUk5seWZkazI4MGZqSUVFZFVlWnBCdk42TGlpdjlXcDF5Ni9KeXc0PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzYwMDAwMDAxCWZpbGU6a3l0cml4LXRlc3QtcGF5bG9hZC1sYXJnZQlwcmVoYXNoZWQKaU1zd2RRN1FLTjUwUkVYeU1zZnJTODNKMXVsZytLTUpHVnh2MFRLaWl4REdkTnBmRHVDcHUzT3g4K244bG81T0gzRGs4U2JmWUNFaFViT09yUnJqREE9PQo=";

/// The payload the release publishes, generated the way the fixture signed it.
fn payload() -> Bytes {
    Bytes::from(
        (0..LENGTH)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<u8>>(),
    )
}

/// A response body that hands out its frames one at a time, pausing between them.
///
/// The pause is what makes a download that is cancelled observably mid-transfer
/// rather than after the fact: without it the whole payload is handed to the
/// kernel at once and there is nothing left to interrupt.
struct PacedBody {
    frames: VecDeque<Frame<Bytes>>,
    pace: Duration,
    wait: Option<Pin<Box<Sleep>>>,
    length: Option<u64>,
}

impl PacedBody {
    fn new(payload: Bytes, pace: Duration, length: Option<u64>) -> Self {
        let frames = payload
            .chunks(FRAME)
            .map(|part| Frame::data(Bytes::copy_from_slice(part)))
            .collect();

        Self {
            frames,
            pace,
            wait: None,
            length,
        }
    }

    /// A body of one frame, sent as fast as the connection takes it.
    fn whole(payload: Bytes) -> Self {
        let length = payload.len() as u64;

        Self::new(payload, Duration::ZERO, Some(length))
    }
}

impl Body for PacedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if let Some(wait) = this.wait.as_mut() {
            match wait.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => this.wait = None,
            }
        }
        let Some(frame) = this.frames.pop_front() else {
            return Poll::Ready(None);
        };
        if !this.frames.is_empty() {
            this.wait = Some(Box::pin(tokio::time::sleep(this.pace)));
        }

        Poll::Ready(Some(Ok(frame)))
    }

    fn size_hint(&self) -> SizeHint {
        match self.length {
            Some(length) => SizeHint::with_exact(length),
            None => SizeHint::default(),
        }
    }
}

fn empty() -> PacedBody {
    PacedBody::whole(Bytes::new())
}

fn answer(status: StatusCode, body: PacedBody, location: Option<&str>) -> Response<PacedBody> {
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
                "signature": SIGNATURE,
                "format": "nsis",
            },
        },
    });

    Bytes::from(serde_json::to_vec(&published).expect("the manifest is JSON"))
}

/// Starts a release server: the metadata behind a redirect, and the payload behind another.
///
/// The payload is announced at `payload_path`, which a test points at a path the
/// server does not serve when what it is about is a release that is gone.
async fn spawn_release_server(
    payload_path: &'static str,
    pace: Duration,
    length: Option<u64>,
) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("a loopback port is free");
    let address = listener.local_addr().expect("the listener has an address");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let service = service_fn(move |request: Request<Incoming>| {
                let path = request.uri().path().to_owned();
                async move {
                    let response = match path.as_str() {
                        "/latest.json" => {
                            answer(StatusCode::FOUND, empty(), Some("/releases/latest.json"))
                        }
                        // A relative location, which is what GitHub sends and what the
                        // client has to resolve against the request it made.
                        "/releases/latest.json" => answer(
                            StatusCode::OK,
                            PacedBody::whole(metadata(address, payload_path)),
                            None,
                        ),
                        "/payload" => answer(
                            StatusCode::TEMPORARY_REDIRECT,
                            empty(),
                            Some("/releases/payload"),
                        ),
                        "/releases/payload" => answer(
                            StatusCode::OK,
                            PacedBody::new(payload(), pace, length),
                            None,
                        ),
                        _ => answer(
                            StatusCode::NOT_FOUND,
                            PacedBody::whole(Bytes::from_static(b"not found")),
                            None,
                        ),
                    };

                    Ok::<_, Infallible>(response)
                }
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
async fn a_release_should_be_read_downloaded_and_verified_end_to_end() {
    let address = spawn_release_server("/payload", Duration::ZERO, None).await;
    let hub = transport();
    let runtime = Runtime::for_test();
    let config = Config::default();
    let cancellation = hub.cancellation().child_token();
    let endpoint = endpoint(address);

    let release = fetch::fetch_release(&hub, &config, &runtime, &endpoint, &cancellation)
        .await
        .expect("the metadata is read through its redirect")
        .expect("a newer release is offered");
    assert_eq!(release.version, semver::Version::new(9, 9, 9));
    assert_eq!(release.asset.format, ReleaseFormat::Nsis);
    assert_eq!(release.asset.signature.as_str(), SIGNATURE);
    assert_eq!(
        release.asset.url.as_str(),
        format!("http://{address}/payload"),
        "the payload is what the metadata named, not what was asked for"
    );

    let reported = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&reported);
    let bytes = fetch::download(
        &hub,
        &config,
        &runtime,
        &release,
        &cancellation,
        move |read, total| {
            recorder
                .lock()
                .expect("the recorder is never poisoned")
                .push((read, total));
        },
    )
    .await
    .expect("the payload is downloaded through its redirect");

    assert_eq!(
        bytes,
        payload(),
        "the bytes that arrived are the bytes served"
    );
    verify::verify(&bytes, release.asset.signature.as_str(), PUBKEY)
        .expect("the payload verifies against the key it was signed with");

    let reported = reported.lock().expect("the recorder is never poisoned");
    assert!(
        reported.len() > 2,
        "a paced payload is reported frame by frame, not once"
    );
    assert!(
        reported.windows(2).all(|pair| pair[0].0 <= pair[1].0),
        "progress never goes backwards: {reported:?}"
    );
    assert_eq!(
        reported.first(),
        Some(&(0, None)),
        "a chunked response names no length, and says so before the first byte"
    );
    assert_eq!(
        reported.last().map(|(read, _)| *read),
        Some(bytes.len() as u64),
        "the last report is the whole payload"
    );
}

#[tokio::test]
async fn a_download_should_be_cancellable_mid_transfer_and_keep_nothing() {
    let address =
        spawn_release_server("/payload", Duration::from_millis(50), Some(LENGTH as u64)).await;
    let hub = transport();
    let runtime = Runtime::for_test();
    let config = Config::default();
    let cancellation = hub.cancellation().child_token();
    let endpoint = endpoint(address);

    let release = fetch::fetch_release(&hub, &config, &runtime, &endpoint, &cancellation)
        .await
        .expect("the metadata is read")
        .expect("a newer release is offered");

    let reported = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&reported);
    let cancelled = cancellation.clone();
    let outcome = fetch::download(
        &hub,
        &config,
        &runtime,
        &release,
        &cancellation,
        move |read, _| {
            recorder
                .lock()
                .expect("the recorder is never poisoned")
                .push(read);
            // The user cancels once the first frame has been accepted, which is
            // while the rest of the payload is still being sent.
            if read > 0 {
                cancelled.cancel();
            }
        },
    )
    .await;

    assert!(
        matches!(
            outcome,
            Err(UpdaterError::Transport(UpdateRequestError::Read(
                TransportError::Cancelled
            )))
        ),
        "a cancelled download reports the cancellation rather than the bytes it had: {outcome:?}"
    );
    let reported = reported.lock().expect("the recorder is never poisoned");
    let last = *reported.last().expect("the download reported its start");
    assert!(
        last > 0 && last < LENGTH as u64,
        "the download was stopped part way through, not before or after it: {last}"
    );
}

#[tokio::test]
async fn a_payload_the_server_does_not_have_should_be_an_error_rather_than_bytes() {
    let address = spawn_release_server("/gone", Duration::ZERO, None).await;
    let hub = transport();
    let runtime = Runtime::for_test();
    let config = Config::default();
    let cancellation = hub.cancellation().child_token();
    let endpoint = endpoint(address);

    let release = fetch::fetch_release(&hub, &config, &runtime, &endpoint, &cancellation)
        .await
        .expect("the metadata is read")
        .expect("a newer release is offered");
    let outcome =
        fetch::download(&hub, &config, &runtime, &release, &cancellation, |_, _| {}).await;

    assert!(
        matches!(
            outcome,
            Err(UpdaterError::Transport(UpdateRequestError::Status {
                status: StatusCode::NOT_FOUND
            }))
        ),
        "a release that is gone is a failure, not an empty payload: {outcome:?}"
    );
}

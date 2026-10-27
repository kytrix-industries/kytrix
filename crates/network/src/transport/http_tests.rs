use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use app_config::network::ResolvedProxy;
use bytes::Bytes;
use smol_str::SmolStr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Notify, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{
    test_support::{Socks5Proxy, spawn_static_http_server},
    transport::{
        error::TransportError,
        http::{
            HttpClientClass, HttpClientPool, read_response_body, read_response_body_with_progress,
        },
        hub::TransportPolicy,
    },
};

const MIB: usize = 1024 * 1024;

fn plaintext_policy() -> TransportPolicy {
    TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    }
}

fn pool() -> HttpClientPool {
    HttpClientPool::new(plaintext_policy())
}

/// The body budget of the class the over-limit paths are exercised against.
///
/// The metadata class has the smallest budget in the pool, so a fixture one byte
/// over it is the smallest fixture that proves a limit is enforced. Taking the
/// number from the class rather than writing it out keeps those tests true when the
/// budget moves.
fn metadata_limit() -> usize {
    HttpClientClass::UpdaterMetadata.response_body_limit()
}

#[test]
fn client_classes_have_fixed_body_and_redirect_limits() {
    // The updater metadata endpoint serves `latest.json`: both language files of the
    // release notes plus six signed platform entries. The release job asserts the
    // assembled file stays under this limit, so the two numbers move together.
    assert_eq!(
        HttpClientClass::UpdaterMetadata.response_body_limit(),
        64 * 1024
    );
    for class in [
        HttpClientClass::Public,
        HttpClientClass::Private,
        HttpClientClass::KytrixService,
    ] {
        assert_eq!(class.response_body_limit(), 30 * MIB, "{class:?}");
        assert_eq!(class.redirect_limit(), 0, "{class:?}");
    }
    assert_eq!(
        HttpClientClass::XAccountMonitoringMedia.response_body_limit(),
        64 * MIB
    );
    assert_eq!(HttpClientClass::XAccountMonitoringMedia.redirect_limit(), 5);
    assert_eq!(HttpClientClass::UpdaterDownload.redirect_limit(), 10);
    assert_eq!(
        HttpClientClass::UpdaterDownload.response_body_limit(),
        1024 * MIB
    );
}

async fn spawn_keep_alive_server() -> (
    SocketAddr,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    CancellationToken,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task_accepts = Arc::clone(&accepts);
    let task_requests = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                biased;
                () = task_cancellation.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else { break };
            task_accepts.fetch_add(1, Ordering::AcqRel);
            let connection_cancellation = task_cancellation.clone();
            let connection_requests = Arc::clone(&task_requests);
            tokio::spawn(async move {
                let mut buffered = Vec::new();
                loop {
                    while !buffered.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let mut chunk = [0_u8; 1_024];
                        let read = tokio::select! {
                            biased;
                            () = connection_cancellation.cancelled() => return,
                            read = stream.read(&mut chunk) => read.unwrap_or_default(),
                        };
                        if read == 0 {
                            return;
                        }
                        buffered.extend_from_slice(&chunk[..read]);
                    }
                    let header_end = buffered
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .unwrap()
                        + 4;
                    buffered.drain(..header_end);
                    connection_requests.fetch_add(1, Ordering::AcqRel);
                    if stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                        )
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (address, accepts, requests, cancellation, task)
}

async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
    timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::Acquire) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("HTTP observation should reach the expected count");
}

#[tokio::test]
async fn the_pool_reuses_tcp_connections_only_for_the_exact_proxy_and_class() {
    let (address, accepts, requests, cancellation, server) = spawn_keep_alive_server().await;
    let proxy = Socks5Proxy::start(None).await;
    let proxied = ResolvedProxy::Socks5 {
        host: SmolStr::new_static("127.0.0.1"),
        port: proxy.address.port(),
        credentials: None,
        resolve_dns_at_remote: false,
    };
    let pool = pool();
    let url = format!("http://{address}/reuse");
    let first = pool
        .client(&ResolvedProxy::NoProxy, HttpClientClass::Public)
        .unwrap();
    let second = pool
        .client(&ResolvedProxy::NoProxy, HttpClientClass::Public)
        .unwrap();

    assert_eq!(
        first.get(&url).send().await.unwrap().text().await.unwrap(),
        "ok"
    );
    assert_eq!(
        second.get(&url).send().await.unwrap().text().await.unwrap(),
        "ok"
    );
    wait_for_count(&requests, 2).await;
    assert_eq!(accepts.load(Ordering::Acquire), 1);

    let different_class = pool
        .client(&ResolvedProxy::NoProxy, HttpClientClass::Private)
        .unwrap();
    assert_eq!(
        different_class
            .get(&url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    wait_for_count(&requests, 3).await;
    assert_eq!(accepts.load(Ordering::Acquire), 2);

    let different_proxy = pool.client(&proxied, HttpClientClass::Public).unwrap();
    assert_eq!(
        different_proxy
            .get(&url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    wait_for_count(&requests, 4).await;
    assert_eq!(accepts.load(Ordering::Acquire), 3);

    drop((first, second, different_class, different_proxy, pool));
    cancellation.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn a_plaintext_policy_is_required_for_plain_http() {
    let (address, server) = spawn_static_http_server("ok").await;
    let strict = HttpClientPool::new(TransportPolicy::default());
    let error = strict
        .client(&ResolvedProxy::NoProxy, HttpClientClass::KytrixService)
        .unwrap()
        .get(format!("http://{address}/oauth/token"))
        .send()
        .await
        .unwrap_err();
    assert!(error.is_builder());

    let response = pool()
        .client(&ResolvedProxy::NoProxy, HttpClientClass::KytrixService)
        .unwrap()
        .get(format!("http://{address}/oauth/token"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    server.await.unwrap();
}

async fn response(body: &[u8], content_length: Option<usize>) -> reqwest::Response {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = Arc::<[u8]>::from(body);
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0; 256];
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let header = content_length.map_or_else(
            || "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_owned(),
            |length| {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
            },
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(&body).await.unwrap();
    });
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_response_reader_rejects_a_large_content_length_before_reading() {
    let limit = metadata_limit();
    let response = response(&[], Some(limit + 1)).await;

    let result = read_response_body(
        response,
        HttpClientClass::UpdaterMetadata,
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result, Err(TransportError::ResponseBodyTooLarge { limit }));
}

#[tokio::test]
async fn the_response_reader_enforces_the_limit_without_content_length() {
    let limit = metadata_limit();
    let response = response(&vec![b'x'; limit + 1], None).await;

    let result = read_response_body(
        response,
        HttpClientClass::UpdaterMetadata,
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result, Err(TransportError::ResponseBodyTooLarge { limit }));
}

#[tokio::test]
async fn the_response_reader_returns_bounded_bytes() {
    let response = response(b"small", Some(5)).await;

    let result = read_response_body(
        response,
        HttpClientClass::UpdaterMetadata,
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result, Ok(Bytes::from_static(b"small")));
}

#[tokio::test]
async fn an_already_cancelled_read_never_touches_the_body() {
    let response = response(b"small", Some(5)).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result =
        read_response_body(response, HttpClientClass::UpdaterMetadata, &cancellation).await;

    assert_eq!(result, Err(TransportError::Cancelled));
}

#[tokio::test]
async fn a_stalled_body_read_is_cancelled_mid_read() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_sender, started_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 512];
        let _ = socket.read(&mut request).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
            .await;
        let _ = started_sender.send(());
        std::future::pending::<()>().await;
    });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}/stalled"))
        .send()
        .await
        .unwrap();
    started_receiver.await.unwrap();
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let result = timeout(
        Duration::from_secs(2),
        read_response_body(response, HttpClientClass::Public, &cancellation),
    )
    .await;

    assert_eq!(result, Ok(Err(TransportError::Cancelled)));
}

/// Records every progress report so a test can assert the call sequence.
#[derive(Clone, Default)]
struct ProgressLog(Arc<Mutex<Vec<(u64, Option<u64>)>>>);

impl ProgressLog {
    fn record(&self, bytes_read: u64, total: Option<u64>) {
        self.0.lock().unwrap().push((bytes_read, total));
    }

    fn entries(&self) -> Vec<(u64, Option<u64>)> {
        self.0.lock().unwrap().clone()
    }
}

/// Serves one response whose body is written in `parts`, waiting for `progress`
/// between parts so each part after the first is written only once the reader has
/// reported the previous one. `content_length` selects the framing: `Some`
/// declares the length, `None` sends a chunked body terminated by the last chunk.
async fn streamed_response(
    parts: &[&[u8]],
    content_length: Option<usize>,
    progress: Arc<Notify>,
) -> reqwest::Response {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let parts = parts
        .iter()
        .map(|part| Arc::<[u8]>::from(*part))
        .collect::<Vec<_>>();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0; 256];
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let chunked = content_length.is_none();
        let header = content_length.map_or_else(
            || {
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                    .to_owned()
            },
            |length| {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
            },
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        for part in parts {
            if chunked {
                socket
                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                    .await
                    .unwrap();
            }
            socket.write_all(&part).await.unwrap();
            if chunked {
                socket.write_all(b"\r\n").await.unwrap();
            }
            progress.notified().await;
        }
        if chunked {
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        }
    });
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_chunked_body_reports_cumulative_progress() {
    let parts: [&[u8]; 3] = [b"alpha ", b"beta ", b"gamma"];
    let gate = Arc::new(Notify::new());
    let response = streamed_response(&parts, None, Arc::clone(&gate)).await;
    let progress = ProgressLog::default();
    let recorder = progress.clone();
    let notifier = Arc::clone(&gate);

    let result = timeout(
        Duration::from_secs(2),
        read_response_body_with_progress(
            response,
            HttpClientClass::UpdaterDownload,
            &CancellationToken::new(),
            |bytes_read, total| {
                recorder.record(bytes_read, total);
                if bytes_read > 0 {
                    notifier.notify_one();
                }
            },
        ),
    )
    .await
    .expect("every part should be reported so the server may write the next one");
    let expected = parts.concat();

    assert_eq!(result, Ok(Bytes::from(expected.clone())));
    let entries = progress.entries();
    assert_eq!(entries.first(), Some(&(0, None)));
    assert_eq!(entries.last(), Some(&(expected.len() as u64, None)));
    assert!(entries.iter().all(|entry| entry.1.is_none()));
    assert!(entries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    assert!(
        entries
            .iter()
            .any(|entry| entry.0 > 0 && entry.0 < expected.len() as u64),
        "progress should be reported before the body ends"
    );
}

#[tokio::test]
async fn progress_reports_the_content_length_as_the_total() {
    let progress = ProgressLog::default();
    let recorder = progress.clone();

    let result = read_response_body_with_progress(
        response(b"small", Some(5)).await,
        HttpClientClass::UpdaterMetadata,
        &CancellationToken::new(),
        |bytes_read, total| recorder.record(bytes_read, total),
    )
    .await;

    assert_eq!(result, Ok(Bytes::from_static(b"small")));
    let entries = progress.entries();
    assert_eq!(entries.first(), Some(&(0, Some(5))));
    assert_eq!(entries.last(), Some(&(5, Some(5))));
    assert!(entries.iter().all(|entry| entry.1 == Some(5)));
}

#[tokio::test]
async fn progress_reports_no_total_without_a_content_length() {
    let progress = ProgressLog::default();
    let recorder = progress.clone();

    let result = read_response_body_with_progress(
        response(b"small", None).await,
        HttpClientClass::UpdaterMetadata,
        &CancellationToken::new(),
        |bytes_read, total| recorder.record(bytes_read, total),
    )
    .await;

    assert_eq!(result, Ok(Bytes::from_static(b"small")));
    let entries = progress.entries();
    assert_eq!(entries.first(), Some(&(0, None)));
    assert_eq!(entries.last(), Some(&(5, None)));
    assert!(entries.iter().all(|entry| entry.1.is_none()));
}

#[tokio::test]
async fn the_progress_reader_rejects_a_large_content_length_without_reporting() {
    let limit = metadata_limit();
    let progress = ProgressLog::default();
    let recorder = progress.clone();

    let result = read_response_body_with_progress(
        response(&[], Some(limit + 1)).await,
        HttpClientClass::UpdaterMetadata,
        &CancellationToken::new(),
        |bytes_read, total| recorder.record(bytes_read, total),
    )
    .await;

    assert_eq!(result, Err(TransportError::ResponseBodyTooLarge { limit }));
    assert!(progress.entries().is_empty());
}

#[tokio::test]
async fn the_progress_reader_rejects_a_streaming_body_over_the_limit() {
    let limit = metadata_limit();
    let progress = ProgressLog::default();
    let recorder = progress.clone();

    let result = read_response_body_with_progress(
        response(&vec![b'x'; limit + 1], None).await,
        HttpClientClass::UpdaterMetadata,
        &CancellationToken::new(),
        |bytes_read, total| recorder.record(bytes_read, total),
    )
    .await;

    assert_eq!(result, Err(TransportError::ResponseBodyTooLarge { limit }));
    let entries = progress.entries();
    assert_eq!(entries.first(), Some(&(0, None)));
    assert!(entries.iter().all(|entry| entry.0 <= limit as u64));
    assert!(entries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
}

#[tokio::test]
async fn a_streaming_read_cancelled_mid_body_reports_cancelled() {
    let response = streamed_response(&[b"ab"], Some(4), Arc::new(Notify::new())).await;
    let progress = ProgressLog::default();
    let recorder = progress.clone();
    let cancellation = CancellationToken::new();
    let stopper = cancellation.clone();

    let result = timeout(
        Duration::from_secs(2),
        read_response_body_with_progress(
            response,
            HttpClientClass::UpdaterDownload,
            &cancellation,
            |bytes_read, total| {
                recorder.record(bytes_read, total);
                if bytes_read > 0 {
                    stopper.cancel();
                }
            },
        ),
    )
    .await;

    assert_eq!(result, Ok(Err(TransportError::Cancelled)));
    assert_eq!(progress.entries(), vec![(0, Some(4)), (2, Some(4))]);
}

#[tokio::test]
async fn the_start_of_the_read_is_reported_before_the_first_body_byte() {
    // The first part is empty, so the server sends the headers and then waits:
    // the body cannot be written until the reader has announced the total, and
    // the reader cannot read a byte until the body is written. Only a report made
    // before the first read can open the gate; announcing it any later deadlocks
    // the exchange and the timeout reports it.
    let gate = Arc::new(Notify::new());
    let response = streamed_response(&[b"", b"abcd"], Some(4), Arc::clone(&gate)).await;
    let progress = ProgressLog::default();
    let recorder = progress.clone();
    let notifier = Arc::clone(&gate);

    let result = timeout(
        Duration::from_secs(2),
        read_response_body_with_progress(
            response,
            HttpClientClass::UpdaterDownload,
            &CancellationToken::new(),
            |bytes_read, total| {
                recorder.record(bytes_read, total);
                notifier.notify_one();
            },
        ),
    )
    .await
    .expect("the total must be reported from the headers, before the body arrives");

    assert_eq!(result, Ok(Bytes::from_static(b"abcd")));
    assert_eq!(
        progress.entries(),
        vec![(0, Some(4)), (4, Some(4))],
        "the start is reported once from the headers and then once per accepted chunk"
    );
}

#[tokio::test]
async fn a_progress_read_that_starts_cancelled_reports_nothing() {
    let progress = ProgressLog::default();
    let recorder = progress.clone();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = read_response_body_with_progress(
        response(b"small", Some(5)).await,
        HttpClientClass::UpdaterMetadata,
        &cancellation,
        |bytes_read, total| recorder.record(bytes_read, total),
    )
    .await;

    assert_eq!(result, Err(TransportError::Cancelled));
    assert!(progress.entries().is_empty());
}

#[tokio::test]
async fn a_body_that_ends_before_its_content_length_is_never_reported_as_complete() {
    let progress = ProgressLog::default();
    let recorder = progress.clone();

    let result = read_response_body_with_progress(
        response(b"ab", Some(4)).await,
        HttpClientClass::UpdaterDownload,
        &CancellationToken::new(),
        |bytes_read, total| recorder.record(bytes_read, total),
    )
    .await;

    // A body that stops two bytes short of its own `Content-Length` is a failed
    // read, not a short one: the progress must show what arrived and never the
    // advertised total, so a caller can never mistake it for a finished download.
    assert_eq!(result, Err(TransportError::ResponseReadFailed));
    let entries = progress.entries();
    assert_eq!(entries.first(), Some(&(0, Some(4))));
    assert_eq!(entries.last(), Some(&(2, Some(4))));
    assert!(entries.iter().all(|entry| entry.1 == Some(4)));
    assert!(entries.iter().all(|entry| entry.0 < 4));
}

#[tokio::test]
async fn read_response_body_returns_the_same_bytes_as_the_progress_reader() {
    let body = (0_u8..=255).cycle().take(300).collect::<Vec<_>>();
    let class = HttpClientClass::UpdaterDownload;

    let plain = read_response_body(
        response(&body, Some(body.len())).await,
        class,
        &CancellationToken::new(),
    )
    .await;
    let progress = ProgressLog::default();
    let recorder = progress.clone();
    let reported = read_response_body_with_progress(
        response(&body, Some(body.len())).await,
        class,
        &CancellationToken::new(),
        |bytes_read, total| recorder.record(bytes_read, total),
    )
    .await;

    assert_eq!(plain, Ok(Bytes::from(body.clone())));
    assert_eq!(reported, plain);
    assert_eq!(
        progress.entries().last(),
        Some(&(body.len() as u64, Some(body.len() as u64)))
    );
}

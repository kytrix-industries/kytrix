use std::{
    io,
    path::Path,
    sync::{Arc, Mutex},
};

use app_config::network::Config;
use network::{proxy::resolve_async_cancellable, transport::hub::TransportHub};
use reqwest::{
    Response, StatusCode,
    header::{ACCEPT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, LOCATION, RANGE},
};
use smol_str::{SmolStr, format_smolstr};
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    runtime::Handle,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    error::{DownloadError, RangeFailure, RedirectFailure},
    progress::Progress,
    request::DownloadRequest,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Validator {
    Etag(SmolStr),
    LastModified(SmolStr),
}

impl Validator {
    fn header_value(&self) -> &str {
        match self {
            Self::Etag(value) | Self::LastModified(value) => value,
        }
    }
}

pub(crate) async fn download(
    transport: &TransportHub,
    config: &Config,
    runtime: &Handle,
    request: &DownloadRequest,
    fresh: bool,
    validator: Arc<Mutex<Option<Validator>>>,
    cancellation: &CancellationToken,
    progress: &Progress,
) -> Result<u64, DownloadError> {
    let outcome = attempt(
        transport,
        config,
        runtime,
        request,
        fresh,
        validator,
        cancellation,
        progress,
    )
    .await;
    if let Err(error) = &outcome {
        log::debug!(
            "the download to {:?} failed ({:?}): {error:?}",
            request.destination,
            error.failure(),
        );
    }

    outcome
}

async fn attempt(
    transport: &TransportHub,
    config: &Config,
    runtime: &Handle,
    request: &DownloadRequest,
    fresh: bool,
    validator: Arc<Mutex<Option<Validator>>>,
    cancellation: &CancellationToken,
    progress: &Progress,
) -> Result<u64, DownloadError> {
    if cancellation.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }

    let Some(part) = crate::partial::path(&request.destination) else {
        return Err(DownloadError::InvalidDestination {
            path: request.destination.clone(),
        });
    };
    if let Some(parent) = request.destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(DownloadError::Io)?;
    }

    let mut offset = if fresh { 0 } else { part_length(&part).await? };
    if let Some(expected) = request.expected_size {
        if offset == expected {
            if offset == 0 {
                File::create(&part).await.map_err(DownloadError::Io)?;
            }
            replace_file(&part, &request.destination).await?;
            return Ok(offset);
        }
        if offset > expected {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(DownloadError::SizeMismatch {
                expected,
                actual: offset,
            });
        }
    }

    let mut file = if fresh {
        File::create(&part).await.map_err(DownloadError::Io)?
    } else {
        open_part(&part, &mut offset).await?
    };
    let _operation = transport
        .begin_operation()
        .map_err(DownloadError::Transport)?;
    let if_range = current_validator(&validator);
    let limit = request.class.redirect_limit();
    let size_limit = request.class.response_body_limit() as u64;
    let mut url = request.url.clone();
    let mut followed = 0_usize;

    let mut response = loop {
        let route = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(DownloadError::Cancelled),
            route = resolve_async_cancellable(
                config,
                &request.target,
                &url,
                cancellation.child_token(),
                runtime,
            ) => route.map_err(DownloadError::Route)?,
        };
        let client = transport
            .http()
            .client(&route, request.class)
            .map_err(DownloadError::Transport)?;
        let mut builder = client.get(url.clone()).header(ACCEPT_ENCODING, "identity");
        if offset > 0 {
            let value = format_smolstr!("bytes={offset}-");
            builder = builder.header(RANGE, value.as_str());
            if let Some(validator) = if_range.as_ref() {
                builder = builder.header(IF_RANGE, validator.header_value());
            }
        }
        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(DownloadError::Cancelled),
            response = builder.send() => response.map_err(DownloadError::Request)?,
        };

        let status = response.status();
        if !status.is_redirection() || status == StatusCode::NOT_MODIFIED {
            break response;
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok());
        url = redirect_target(&url, location, followed, limit)?;
        followed += 1;
    };

    let body_length = response.content_length();
    let status = response.status();
    let mut partial_range = None;
    let total = match status {
        StatusCode::OK => {
            if offset > 0 {
                offset = 0;
                drop(file);
                file = File::create(&part).await.map_err(DownloadError::Io)?;
            }
            store_validator(&validator, &response);
            request.expected_size.or(body_length)
        }
        StatusCode::PARTIAL_CONTENT => {
            if offset == 0 {
                return Err(DownloadError::Range(RangeFailure::Unsolicited));
            }
            let value = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .ok_or(DownloadError::Range(RangeFailure::Missing))?;
            let range =
                content_range(value).ok_or(DownloadError::Range(RangeFailure::Malformed))?;
            if range.start != offset {
                return Err(DownloadError::Range(RangeFailure::StartMismatch {
                    expected: offset,
                    actual: range.start,
                }));
            }
            if let Some(announced) = body_length
                && announced != range.length()
            {
                return Err(DownloadError::Range(RangeFailure::LengthMismatch {
                    range: range.length(),
                    announced,
                }));
            }
            store_validator(&validator, &response);
            partial_range = Some(range);
            request
                .expected_size
                .or(range.total)
                .or(body_length.map(|length| offset.saturating_add(length)))
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            drop(file);
            File::create(&part).await.map_err(DownloadError::Io)?;
            return Err(DownloadError::Range(RangeFailure::Unsatisfiable));
        }
        _ => return Err(DownloadError::Status { status }),
    };

    if body_length.is_some_and(|length| length > size_limit)
        || total.is_some_and(|total| total > size_limit)
    {
        drop(file);
        let _ = tokio::fs::remove_file(&part).await;
        return Err(DownloadError::TooLarge { limit: size_limit });
    }

    progress.opened_at(offset, total);
    let mut written = offset;

    loop {
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                let _ = file.flush().await;
                return Err(DownloadError::Cancelled);
            }
            chunk = response.chunk() => chunk.map_err(DownloadError::Read)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        written = written.saturating_add(chunk.len() as u64);
        if let Some(expected) = request.expected_size
            && written > expected
        {
            drop(file);
            let _ = File::create(&part).await;
            return Err(DownloadError::SizeMismatch {
                expected,
                actual: written,
            });
        }
        if let Some(range) = partial_range
            && written > range.end.saturating_add(1)
        {
            drop(file);
            let _ = File::create(&part).await;
            return Err(DownloadError::Range(RangeFailure::BodyMismatch {
                expected: range.length(),
                actual: written.saturating_sub(offset),
            }));
        }
        if written > size_limit {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            return Err(DownloadError::TooLarge { limit: size_limit });
        }
        file.write_all(&chunk).await.map_err(DownloadError::Io)?;
        progress.wrote(written);
    }

    file.flush().await.map_err(DownloadError::Io)?;
    if let Some(expected) = total
        && written != expected
    {
        return Err(DownloadError::SizeMismatch {
            expected,
            actual: written,
        });
    }
    if let Some(range) = partial_range
        && written != range.end.saturating_add(1)
    {
        return Err(DownloadError::Range(RangeFailure::BodyMismatch {
            expected: range.length(),
            actual: written.saturating_sub(offset),
        }));
    }
    drop(file);
    replace_file(&part, &request.destination).await?;

    Ok(written)
}

async fn part_length(part: &Path) -> Result<u64, DownloadError> {
    match tokio::fs::metadata(part).await {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(DownloadError::Io(error)),
    }
}

async fn open_part(part: &Path, offset: &mut u64) -> Result<File, DownloadError> {
    match *offset {
        0 => File::create(part).await.map_err(DownloadError::Io),
        _ => match OpenOptions::new().append(true).open(part).await {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                *offset = 0;
                File::create(part).await.map_err(DownloadError::Io)
            }
            Err(error) => Err(DownloadError::Io(error)),
        },
    }
}

fn redirect_target(
    current: &Url,
    location: Option<&str>,
    followed: usize,
    limit: usize,
) -> Result<Url, DownloadError> {
    let Some(location) = location else {
        return Err(DownloadError::Redirect(RedirectFailure::MissingLocation));
    };
    if followed >= limit {
        return Err(DownloadError::Redirect(RedirectFailure::HopLimit));
    }
    let next = current
        .join(location)
        .map_err(|_| DownloadError::Redirect(RedirectFailure::MalformedLocation))?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(DownloadError::Redirect(RedirectFailure::Scheme));
    }
    if current.scheme() == "https" && next.scheme() != "https" {
        return Err(DownloadError::Redirect(RedirectFailure::Downgrade));
    }

    Ok(next)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

impl ContentRange {
    fn length(self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

fn content_range(value: &str) -> Option<ContentRange> {
    let (unit, range) = value.trim().split_once(char::is_whitespace)?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = range.split_once('/')?;
    let (start, end) = range.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    if let Some(total) = total
        && end >= total
    {
        return None;
    }

    Some(ContentRange { start, end, total })
}

fn response_validator(response: &Response) -> Option<Validator> {
    if let Some(etag) = header(response, ETAG)
        && !etag
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("W/"))
    {
        return Some(Validator::Etag(SmolStr::new(etag)));
    }

    header(response, LAST_MODIFIED).map(|value| Validator::LastModified(SmolStr::new(value)))
}

fn current_validator(slot: &Mutex<Option<Validator>>) -> Option<Validator> {
    match slot.lock() {
        Ok(current) => current.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn store_validator(slot: &Mutex<Option<Validator>>, response: &Response) {
    let Some(next) = response_validator(response) else {
        return;
    };
    match slot.lock() {
        Ok(mut current) => *current = Some(next),
        Err(poisoned) => *poisoned.into_inner() = Some(next),
    }
}

fn header(response: &Response, name: reqwest::header::HeaderName) -> Option<&str> {
    response.headers().get(name)?.to_str().ok()
}

async fn replace_file(part: &Path, destination: &Path) -> Result<(), DownloadError> {
    match tokio::fs::remove_file(destination).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(DownloadError::Io(error)),
    }
    tokio::fs::rename(part, destination)
        .await
        .map_err(DownloadError::Io)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        net::{Ipv4Addr, SocketAddr},
        path::{Path, PathBuf},
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
        time::Duration,
    };

    use app_config::network::{Config, RouteTarget, ServiceTarget};
    use hyper::{
        Request, Response, StatusCode,
        body::{Body, Bytes, Frame, Incoming, SizeHint},
        header::{CONTENT_RANGE, ETAG, LAST_MODIFIED, LOCATION, TRANSFER_ENCODING},
        server::conn::http1,
        service::service_fn,
    };
    use hyper_util::rt::TokioIo;
    use network::transport::{
        http::HttpClientClass,
        hub::{TransportHub, TransportPolicy},
    };
    use smol_str::SmolStr;
    use tempfile::TempDir;
    use tokio::{net::TcpListener, time::Sleep};
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::{ContentRange, Validator, content_range, download, redirect_target};
    use crate::{
        error::{DownloadError, DownloadFailure, RangeFailure, RedirectFailure},
        progress::{Progress, Snapshot},
        request::DownloadRequest,
        test_support::test_handle,
    };

    const FRAME: usize = 16 * 1024;
    const PACE: Duration = Duration::from_millis(25);
    const OVERSIZED: u64 = 1024 * 1024 * 1024 + 1;

    fn payload(length: usize) -> Bytes {
        Bytes::from(
            (0..length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<u8>>(),
        )
    }

    struct TestBody {
        frames: VecDeque<Frame<Bytes>>,
        pace: Duration,
        wait: Option<Pin<Box<Sleep>>>,
        length: u64,
    }

    impl TestBody {
        fn whole(bytes: Bytes) -> Self {
            let length = bytes.len() as u64;
            let frames = if bytes.is_empty() {
                VecDeque::new()
            } else {
                VecDeque::from([Frame::data(bytes)])
            };

            Self {
                frames,
                pace: Duration::ZERO,
                wait: None,
                length,
            }
        }

        fn paced(bytes: Bytes, pace: Duration) -> Self {
            let length = bytes.len() as u64;
            let frames = bytes
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

        fn announcing(bytes: Bytes, length: u64, pace: Duration) -> Self {
            let mut body = Self::paced(bytes, pace);
            body.length = length;

            body
        }

        fn empty() -> Self {
            Self::whole(Bytes::new())
        }
    }

    impl Body for TestBody {
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
            SizeHint::with_exact(self.length)
        }
    }

    #[derive(Clone, Debug)]
    struct Recorded {
        path: String,
        range: Option<String>,
        if_range: Option<String>,
        accept_encoding: Option<String>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ChunkedRange {
        Short,
        Long,
        Exact,
    }

    struct Server {
        payload: Bytes,
        honor_ranges: bool,
        etag: Option<&'static str>,
        last_modified: Option<&'static str>,
        bad_range: bool,
        chunked_range: Option<ChunkedRange>,
        requests: Mutex<Vec<Recorded>>,
    }

    impl Server {
        fn new(payload: Bytes) -> Self {
            Self {
                payload,
                honor_ranges: true,
                etag: None,
                last_modified: None,
                bad_range: false,
                chunked_range: None,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn ignoring_ranges(mut self) -> Self {
            self.honor_ranges = false;
            self
        }

        fn with_etag(mut self, etag: &'static str) -> Self {
            self.etag = Some(etag);
            self
        }

        fn with_last_modified(mut self, last_modified: &'static str) -> Self {
            self.last_modified = Some(last_modified);
            self
        }

        fn with_bad_range(mut self) -> Self {
            self.bad_range = true;
            self
        }

        fn with_chunked_range(mut self, chunked_range: ChunkedRange) -> Self {
            self.chunked_range = Some(chunked_range);
            self
        }

        fn requests(&self) -> Vec<Recorded> {
            self.requests
                .lock()
                .expect("the request log is never poisoned")
                .clone()
        }

        fn answer(&self, request: &Request<Incoming>) -> Response<TestBody> {
            let path = request.uri().path().to_owned();
            let range = header(request, "range");
            let if_range = header(request, "if-range");
            self.requests
                .lock()
                .expect("the request log is never poisoned")
                .push(Recorded {
                    path: path.clone(),
                    range: range.clone(),
                    if_range: if_range.clone(),
                    accept_encoding: header(request, "accept-encoding"),
                });

            match path.as_str() {
                "/redirect" => redirect("/payload"),
                "/bad-redirect" => redirect("file:///etc/passwd"),
                "/payload" => self.payload_response(range.as_deref(), if_range.as_deref(), false),
                "/paced" => self.payload_response(range.as_deref(), if_range.as_deref(), true),
                "/unsolicited" => self.unsolicited_partial_response(),
                "/oversized-range" => self.oversized_range_response(range.as_deref()),
                "/oversized" => self.reply(
                    StatusCode::OK,
                    None,
                    TestBody::announcing(payload(2 * FRAME), OVERSIZED, PACE),
                ),
                "/chunked" => Response::builder()
                    .status(StatusCode::OK)
                    .header(TRANSFER_ENCODING, "chunked")
                    .body(TestBody::whole(payload(2 * 64 * 1024)))
                    .expect("the chunked response is well formed"),
                _ => self.reply(StatusCode::NOT_FOUND, None, TestBody::empty()),
            }
        }

        fn payload_response(
            &self,
            range: Option<&str>,
            if_range: Option<&str>,
            paced: bool,
        ) -> Response<TestBody> {
            let length = self.payload.len() as u64;
            let conditional = match (self.etag, if_range) {
                (Some(etag), Some(sent)) => etag == sent,
                _ => true,
            };
            let start = range
                .and_then(requested_start)
                .filter(|_| self.honor_ranges && conditional);
            if let Some(start) = start {
                if start >= length {
                    return self.reply(
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        Some(format!("bytes */{length}")),
                        TestBody::empty(),
                    );
                }
                if let Some(chunked_range) = self.chunked_range {
                    return self.chunked_response(start, chunked_range);
                }
                let remaining = self.payload.slice(start as usize..);
                let body = if paced {
                    TestBody::paced(remaining, PACE)
                } else {
                    TestBody::whole(remaining)
                };
                let end = if self.bad_range {
                    start + 31
                } else {
                    length - 1
                };

                return self.reply(
                    StatusCode::PARTIAL_CONTENT,
                    Some(format!("bytes {start}-{end}/{length}")),
                    body,
                );
            }
            let body = if paced {
                TestBody::paced(self.payload.clone(), PACE)
            } else {
                TestBody::whole(self.payload.clone())
            };

            self.reply(StatusCode::OK, None, body)
        }

        fn chunked_response(&self, start: u64, chunked_range: ChunkedRange) -> Response<TestBody> {
            let length = self.payload.len() as u64;
            let end = length - 1;
            let mut body = self.payload.slice(start as usize..).to_vec();
            match chunked_range {
                ChunkedRange::Short => {
                    body.pop();
                }
                ChunkedRange::Long => body.push(0xFF),
                ChunkedRange::Exact => {}
            }

            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_RANGE, format!("bytes {start}-{end}/*"))
                .header(TRANSFER_ENCODING, "chunked")
                .body(TestBody::whole(Bytes::from(body)))
                .expect("the chunked partial response is well formed")
        }

        fn unsolicited_partial_response(&self) -> Response<TestBody> {
            let length = self.payload.len() as u64;
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_RANGE, format!("bytes 0-{}/{length}", length - 1))
                .header(TRANSFER_ENCODING, "chunked")
                .body(TestBody::whole(self.payload.clone()))
                .expect("the unsolicited partial response is well formed")
        }

        fn oversized_range_response(&self, range: Option<&str>) -> Response<TestBody> {
            let start = range
                .and_then(requested_start)
                .expect("the test seeds a partial file so it asks for a range");
            let end = start + 1;

            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_RANGE, format!("bytes {start}-{end}/{OVERSIZED}"))
                .header(TRANSFER_ENCODING, "chunked")
                .body(TestBody::whole(payload(2)))
                .expect("the oversized partial response is well formed")
        }

        fn reply(
            &self,
            status: StatusCode,
            content_range: Option<String>,
            body: TestBody,
        ) -> Response<TestBody> {
            let mut builder = Response::builder().status(status);
            if let Some(etag) = self.etag {
                builder = builder.header(ETAG, etag);
            }
            if let Some(last_modified) = self.last_modified {
                builder = builder.header(LAST_MODIFIED, last_modified);
            }
            if let Some(content_range) = content_range {
                builder = builder.header(CONTENT_RANGE, content_range);
            }

            builder.body(body).expect("the response is well formed")
        }
    }

    fn requested_start(value: &str) -> Option<u64> {
        value
            .strip_prefix("bytes=")?
            .strip_suffix('-')?
            .parse()
            .ok()
    }

    fn redirect(location: &str) -> Response<TestBody> {
        Response::builder()
            .status(StatusCode::FOUND)
            .header(LOCATION, location)
            .body(TestBody::empty())
            .expect("the redirect is well formed")
    }

    fn header(request: &Request<Incoming>, name: &str) -> Option<String> {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    async fn spawn(server: Arc<Server>) -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a loopback port is free");
        let address = listener.local_addr().expect("the listener has an address");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let server = Arc::clone(&server);
                        async move { Ok::<_, Infallible>(server.answer(&request)) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        address
    }

    fn transport() -> TransportHub {
        TransportHub::new(TransportPolicy {
            allow_plaintext: true,
            extra_root_certificates: Vec::new(),
        })
    }

    fn request(
        address: SocketAddr,
        path: &str,
        destination: &Path,
        expected_size: Option<u64>,
    ) -> DownloadRequest {
        DownloadRequest {
            url: Url::parse(&format!("http://{address}{path}")).expect("the request URL is a URL"),
            destination: destination.to_path_buf(),
            target: RouteTarget::Service(ServiceTarget::TranslationModelDownload),
            class: HttpClientClass::ModelDownload,
            expected_size,
        }
    }

    fn validator() -> Arc<Mutex<Option<Validator>>> {
        Arc::new(Mutex::new(None))
    }

    fn cancel_when(
        progress: &Arc<Progress>,
        cancellation: &CancellationToken,
        mut reached: impl FnMut(&Snapshot) -> bool + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let progress = Arc::clone(progress);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            while !reached(&progress.snapshot()) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            cancellation.cancel();
        })
    }

    async fn file_content(path: &Path) -> Vec<u8> {
        tokio::fs::read(path)
            .await
            .expect("the destination file is readable")
    }

    async fn file_length(path: &Path) -> u64 {
        tokio::fs::metadata(path)
            .await
            .expect("the partial file exists")
            .len()
    }

    async fn exists(path: &Path) -> bool {
        tokio::fs::try_exists(path)
            .await
            .expect("the path can be inspected")
    }

    fn downloading() -> (TempDir, PathBuf, PathBuf) {
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");

        (directory, destination, part)
    }

    #[tokio::test]
    async fn a_full_download_writes_the_destination_and_removes_the_part() {
        let payload = payload(128 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let cancellation = CancellationToken::new();
        let progress = Progress::default();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        let written = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &cancellation,
            &progress,
        )
        .await
        .expect("the payload downloads");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(file_content(&destination).await, payload.to_vec());
        assert!(
            !exists(&part).await,
            "the partial file is renamed into place"
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].accept_encoding.as_deref(),
            Some("identity"),
            "a download never asks for a decoded body"
        );
        assert_eq!(
            requests[0].range, None,
            "a fresh download asks for nothing it already has"
        );

        let snapshot = progress.snapshot();
        assert_eq!(
            (snapshot.written(), snapshot.total(), snapshot.opened()),
            (payload.len() as u64, Some(payload.len() as u64), true),
            "the whole file is what the transfer leaves in its progress",
        );
    }

    #[tokio::test]
    async fn a_download_should_create_the_directories_its_destination_names() {
        let payload = payload(32 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory
            .path()
            .join("models")
            .join("nested")
            .join("model.bin");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let progress = Progress::default();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        let written = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &progress,
        )
        .await
        .expect("a download creates the directories its destination names");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(file_content(&destination).await, payload.to_vec());
        assert_eq!(
            server.requests().len(),
            1,
            "the download asks for the payload once the directory exists"
        );
    }

    #[tokio::test]
    async fn a_destination_directory_that_cannot_be_created_should_fail_before_the_request() {
        let payload = payload(1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let directory = TempDir::new().expect("a temporary directory is available");
        let blocker = directory.path().join("blocked");
        tokio::fs::write(&blocker, b"a file where a directory is needed")
            .await
            .expect("the blocking file is written");
        let destination = blocker.join("model.bin");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let progress = Progress::default();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        let error = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &progress,
        )
        .await
        .expect_err("a destination under a file cannot be written");

        assert!(
            matches!(error.failure(), DownloadFailure::Io { .. }),
            "a directory that cannot be created is a file-system failure: {error:?}"
        );
        assert!(
            server.requests().is_empty(),
            "the transfer must not ask for a file it has nowhere to put"
        );
        assert!(!progress.snapshot().opened(), "no partial file was opened");
    }

    #[tokio::test]
    async fn a_resumed_download_asks_for_the_bytes_the_part_already_holds() {
        let payload = payload(256 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 100 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let progress = Progress::default();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        let written = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &progress,
        )
        .await
        .expect("the rest of the payload downloads");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(file_content(&destination).await, payload.to_vec());
        assert!(!exists(&part).await);

        let requests = server.requests();
        assert_eq!(requests[0].path, "/payload");
        assert_eq!(
            requests[0].range.as_deref(),
            Some("bytes=102400-"),
            "the request starts where the partial file ends"
        );
        assert_eq!(requests[0].if_range, None, "no validator is known yet");

        let snapshot = progress.snapshot();
        assert_eq!(
            (snapshot.written(), snapshot.total(), snapshot.opened()),
            (payload.len() as u64, Some(payload.len() as u64), true),
            "a resume leaves the whole file in its progress, not just the bytes it added",
        );
    }

    #[tokio::test]
    async fn a_transfer_should_announce_its_resume_point_before_it_reads_the_body() {
        let payload = payload(192 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 64 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let cancellation = CancellationToken::new();
        let progress = Arc::new(Progress::default());
        let watcher = cancel_when(&progress, &cancellation, |snapshot| snapshot.opened());
        let request = request(address, "/paced", &destination, Some(payload.len() as u64));

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &cancellation,
            &progress,
        )
        .await;
        watcher.abort();
        assert!(
            matches!(outcome, Err(DownloadError::Cancelled)),
            "the transfer is stopped as soon as it makes the announcement: {outcome:?}"
        );

        let announced = progress.snapshot();
        assert!(announced.opened(), "the partial file was opened");
        assert_eq!(
            announced.total(),
            Some(payload.len() as u64),
            "the size of the entity is announced with the resume point",
        );
        assert!(
            announced.written() >= prefix as u64 && announced.written() < payload.len() as u64,
            "the resume point is announced before the body is read to its end: {announced:?}",
        );
    }

    #[tokio::test]
    async fn a_paused_download_keeps_the_part_and_resumes_with_a_range() {
        let payload = payload(192 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/paced", &destination, Some(payload.len() as u64));

        let cancellation = CancellationToken::new();
        let progress = Arc::new(Progress::default());
        let watcher = cancel_when(&progress, &cancellation, |snapshot| snapshot.written() > 0);
        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &cancellation,
            &progress,
        )
        .await;
        watcher.abort();
        assert!(
            matches!(outcome, Err(DownloadError::Cancelled)),
            "a paused transfer reports the cancellation: {outcome:?}"
        );

        let partial = file_length(&part).await;
        assert!(partial > 0, "some bytes arrived before the pause");
        assert!(
            partial < payload.len() as u64,
            "the transfer was stopped part way through"
        );
        assert!(
            !exists(&destination).await,
            "a paused download is not a destination file"
        );

        let resumed_progress = Progress::default();
        let resumed = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &resumed_progress,
        )
        .await
        .expect("the transfer resumes from the partial file");

        assert_eq!(resumed, payload.len() as u64);
        assert_eq!(file_content(&destination).await, payload.to_vec());
        assert!(!exists(&part).await);

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let expected_range = format!("bytes={partial}-");
        assert_eq!(
            requests[1].range.as_deref(),
            Some(expected_range.as_str()),
            "the resume starts where the pause left the partial file"
        );
        assert_eq!(
            resumed_progress.snapshot().written(),
            payload.len() as u64,
            "the progress of the resume counts the bytes the first attempt wrote",
        );
    }

    #[tokio::test]
    async fn a_server_that_ignores_the_range_restarts_the_file() {
        let payload = payload(128 * 1024);
        let server = Arc::new(Server::new(payload.clone()).ignoring_ranges());
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 64 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let progress = Progress::default();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        let written = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &progress,
        )
        .await
        .expect("the whole entity downloads again");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(file_content(&destination).await, payload.to_vec());
        assert!(!exists(&part).await);

        let requests = server.requests();
        assert_eq!(
            requests[0].range.as_deref(),
            Some("bytes=65536-"),
            "the client did ask to resume"
        );
        assert_eq!(
            progress.snapshot().written(),
            payload.len() as u64,
            "the restarted entity is written whole rather than appended to the bytes the part held"
        );
    }

    #[tokio::test]
    async fn an_etag_is_remembered_and_sent_with_the_next_range() {
        let payload = payload(192 * 1024);
        let server = Arc::new(Server::new(payload.clone()).with_etag("\"v1\""));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let validator = validator();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            Arc::clone(&validator),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the first download completes");

        assert_eq!(
            *validator.lock().expect("the validator is never poisoned"),
            Some(Validator::Etag(SmolStr::new_static("\"v1\""))),
            "the ETag the server named is remembered"
        );

        let prefix = 80 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator,
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the resume completes");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].range.as_deref(), Some("bytes=81920-"));
        assert_eq!(
            requests[1].if_range.as_deref(),
            Some("\"v1\""),
            "the resume is conditional on the remembered entity"
        );
        assert_eq!(file_content(&destination).await, payload.to_vec());
    }

    #[tokio::test]
    async fn a_changed_validator_makes_the_server_send_the_whole_file() {
        let payload = payload(128 * 1024);
        let first = Arc::new(Server::new(payload.clone()).with_etag("\"v1\""));
        let first_address = spawn(Arc::clone(&first)).await;
        let second = Arc::new(Server::new(payload.clone()).with_etag("\"v2\""));
        let second_address = spawn(Arc::clone(&second)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let validator = validator();

        let started = request(
            first_address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );
        download(
            &hub,
            &config,
            &runtime,
            &started,
            true,
            Arc::clone(&validator),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the first download completes");

        let prefix = 64 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let changed = request(
            second_address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );
        let progress = Progress::default();
        download(
            &hub,
            &config,
            &runtime,
            &changed,
            false,
            validator,
            &CancellationToken::new(),
            &progress,
        )
        .await
        .expect("the changed entity downloads again");

        let requests = second.requests();
        assert_eq!(
            requests[0].if_range.as_deref(),
            Some("\"v1\""),
            "the resume names the entity the partial file was taken from"
        );
        assert_eq!(requests[0].range.as_deref(), Some("bytes=65536-"));
        assert_eq!(
            progress.snapshot().written(),
            payload.len() as u64,
            "the server's 200 restarts the file at zero instead of appending to the part"
        );
        assert_eq!(file_content(&destination).await, payload.to_vec());
    }

    #[tokio::test]
    async fn a_download_shorter_than_expected_fails_and_keeps_the_part() {
        let payload = payload(64 * 1024);
        let expected = payload.len() as u64 + 1024;
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/payload", &destination, Some(expected));

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        match outcome {
            Err(DownloadError::SizeMismatch {
                expected: mismatch,
                actual,
            }) => {
                assert_eq!(mismatch, expected);
                assert_eq!(actual, payload.len() as u64);
            }
            other => panic!("a short download is a size mismatch: {other:?}"),
        }
        assert_eq!(
            file_length(&part).await,
            payload.len() as u64,
            "the bytes that did arrive are the resume point"
        );
        assert!(!exists(&destination).await);
    }

    #[tokio::test]
    async fn a_download_longer_than_expected_fails_and_drops_the_part() {
        let payload = payload(64 * 1024);
        let expected = 32 * 1024;
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/payload", &destination, Some(expected));

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        match outcome {
            Err(DownloadError::SizeMismatch {
                expected: mismatch,
                actual,
            }) => {
                assert_eq!(mismatch, expected);
                assert!(actual > expected);
            }
            other => panic!("an overlong download is a size mismatch: {other:?}"),
        }
        assert_eq!(
            file_length(&part).await,
            0,
            "an oversized partial file cannot be resumed"
        );
        assert!(!exists(&destination).await);
    }

    #[tokio::test]
    async fn an_unsatisfiable_range_truncates_the_part() {
        let payload = payload(64 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        tokio::fs::write(&part, &payload[..])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/payload", &destination, None);

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        assert!(
            matches!(
                outcome,
                Err(DownloadError::Range(RangeFailure::Unsatisfiable))
            ),
            "a range beyond the entity is unusable: {outcome:?}"
        );
        assert_eq!(
            file_length(&part).await,
            0,
            "the next resume starts from zero"
        );
        assert!(!exists(&destination).await);
    }

    #[tokio::test]
    async fn a_missing_payload_is_a_status_failure() {
        let payload = payload(1024);
        let server = Arc::new(Server::new(payload));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, _part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/missing", &destination, None);

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        assert!(
            matches!(
                outcome,
                Err(DownloadError::Status { status }) if status == StatusCode::NOT_FOUND
            ),
            "a status that carries no payload is a failure: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_redirect_keeps_the_range_and_the_identity_encoding() {
        let payload = payload(192 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 90 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(
            address,
            "/redirect",
            &destination,
            Some(payload.len() as u64),
        );

        let written = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the payload downloads through its redirect");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(file_content(&destination).await, payload.to_vec());

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, "/redirect");
        assert_eq!(requests[0].accept_encoding.as_deref(), Some("identity"));
        assert_eq!(requests[1].path, "/payload");
        assert_eq!(
            requests[1].range.as_deref(),
            Some("bytes=92160-"),
            "the range survives the hop"
        );
    }

    #[tokio::test]
    async fn a_redirect_to_a_non_http_scheme_is_refused() {
        let payload = payload(1024);
        let server = Arc::new(Server::new(payload));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, _part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/bad-redirect", &destination, None);

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        assert!(
            matches!(
                outcome,
                Err(DownloadError::Redirect(RedirectFailure::Scheme))
            ),
            "a redirect a request cannot be sent to is refused: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_weak_etag_should_not_make_a_resume_conditional() {
        let payload = payload(96 * 1024);
        let server = Arc::new(Server::new(payload.clone()).with_etag("W/\"weak\""));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let validator = validator();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            Arc::clone(&validator),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the first download completes");

        assert_eq!(
            *validator.lock().expect("the validator is never poisoned"),
            None,
            "a weak entity tag is not a validator a resume can rest on"
        );

        let prefix = 32 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator,
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the resume completes");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].range.as_deref(),
            Some("bytes=32768-"),
            "the range is still asked for"
        );
        assert_eq!(
            requests[1].if_range, None,
            "a weak entity tag must not be sent as an If-Range"
        );
        assert_eq!(file_content(&destination).await, payload.to_vec());
    }

    #[tokio::test]
    async fn a_modification_time_should_make_a_resume_conditional() {
        let last_modified = "Wed, 21 Oct 2015 07:28:00 GMT";
        let payload = payload(96 * 1024);
        let server = Arc::new(Server::new(payload.clone()).with_last_modified(last_modified));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let validator = validator();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            Arc::clone(&validator),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the first download completes");

        assert_eq!(
            *validator.lock().expect("the validator is never poisoned"),
            Some(Validator::LastModified(SmolStr::new_static(
                "Wed, 21 Oct 2015 07:28:00 GMT"
            ))),
            "the modification time the server named is remembered"
        );

        let prefix = 32 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator,
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the resume completes");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].if_range.as_deref(),
            Some(last_modified),
            "the resume is conditional on the remembered modification time"
        );
        assert_eq!(file_content(&destination).await, payload.to_vec());
    }

    #[tokio::test]
    async fn a_download_past_its_class_budget_is_refused_before_it_is_read() {
        let server = Arc::new(Server::new(payload(1024)));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let mut request = request(address, "/oversized", &destination, None);
        request.class = HttpClientClass::UpdaterDownload;

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        match outcome {
            Err(DownloadError::TooLarge { limit }) => assert_eq!(
                limit,
                HttpClientClass::UpdaterDownload.response_body_limit() as u64,
                "the refusal names the budget of the download's class"
            ),
            other => panic!("a response past the class budget is refused: {other:?}"),
        }
        assert!(
            !exists(&part).await,
            "an oversized download leaves no partial file to resume"
        );
        assert!(!exists(&destination).await);
    }

    #[tokio::test]
    async fn a_streamed_body_past_its_class_budget_is_refused() {
        let server = Arc::new(Server::new(payload(1024)));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let mut request = request(address, "/chunked", &destination, None);
        request.class = HttpClientClass::UpdaterMetadata;

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        match outcome {
            Err(DownloadError::TooLarge { limit }) => assert_eq!(
                limit,
                HttpClientClass::UpdaterMetadata.response_body_limit() as u64,
                "the refusal names the budget of the download's class"
            ),
            other => panic!("a streamed body past the class budget is refused: {other:?}"),
        }
        assert!(
            !exists(&part).await,
            "an oversized download leaves no partial file to resume"
        );
    }

    #[tokio::test]
    async fn a_partial_response_that_contradicts_its_range_is_refused() {
        let payload = payload(64 * 1024);
        let server = Arc::new(Server::new(payload.clone()).with_bad_range());
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(
            address,
            "/payload",
            &destination,
            Some(payload.len() as u64),
        );

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        match outcome {
            Err(DownloadError::Range(RangeFailure::LengthMismatch { range, announced })) => {
                assert_eq!(range, 32, "the range names thirty-two bytes");
                assert_eq!(
                    announced,
                    (payload.len() - prefix) as u64,
                    "the body carries everything after the partial file"
                );
            }
            other => panic!("a body that is not its range is refused: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fresh_download_ignores_the_part_it_finds_and_asks_for_everything() {
        let payload = payload(96 * 1024);
        let server = Arc::new(Server::new(payload.clone()));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 32 * 1024;
        tokio::fs::write(&part, vec![0xEE_u8; prefix])
            .await
            .expect("the stale partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/payload", &destination, None);

        let written = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("the whole entity downloads over the stale partial file");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(
            file_content(&destination).await,
            payload.to_vec(),
            "the stale bytes are not stitched onto the download"
        );
        assert!(!exists(&part).await);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].range, None,
            "a fresh download asks for the whole entity"
        );
    }

    #[tokio::test]
    async fn a_fresh_download_refuses_an_unsolicited_partial_response() {
        let payload = payload(64 * 1024);
        let server = Arc::new(Server::new(payload));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        tokio::fs::write(&part, vec![0xEE_u8; 16 * 1024])
            .await
            .expect("the stale partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/unsolicited", &destination, None);

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            true,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        assert!(
            matches!(
                outcome,
                Err(DownloadError::Range(RangeFailure::Unsolicited))
            ),
            "a partial response no range asked for is refused: {outcome:?}"
        );
        assert_eq!(
            file_length(&part).await,
            0,
            "the fresh attempt truncated the stale partial file before the refusal"
        );
        assert!(!exists(&destination).await);
        assert_eq!(
            server.requests()[0].range,
            None,
            "the fresh attempt never asked for a range"
        );
    }

    #[tokio::test]
    async fn a_chunked_partial_response_that_stops_early_keeps_the_part() {
        let payload = payload(128 * 1024);
        let server = Arc::new(Server::new(payload.clone()).with_chunked_range(ChunkedRange::Short));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 100 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/payload", &destination, None);

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        let remaining = (payload.len() - prefix) as u64;
        match outcome {
            Err(DownloadError::Range(RangeFailure::BodyMismatch { expected, actual })) => {
                assert_eq!(
                    expected, remaining,
                    "the range names the rest of the entity"
                );
                assert_eq!(actual, remaining - 1, "the body stopped one byte early");
            }
            other => panic!("a body shorter than its range is refused: {other:?}"),
        }
        assert_eq!(
            file_length(&part).await,
            prefix as u64 + remaining - 1,
            "the bytes that did arrive are the resume point"
        );
        assert!(!exists(&destination).await);
        let expected_range = format!("bytes={prefix}-");
        assert_eq!(
            server.requests()[0].range.as_deref(),
            Some(expected_range.as_str()),
            "the request resumes from the partial file"
        );
    }

    #[tokio::test]
    async fn a_chunked_partial_response_that_runs_long_drops_the_part() {
        let payload = payload(128 * 1024);
        let server = Arc::new(Server::new(payload.clone()).with_chunked_range(ChunkedRange::Long));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 100 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/payload", &destination, None);

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        let remaining = (payload.len() - prefix) as u64;
        match outcome {
            Err(DownloadError::Range(RangeFailure::BodyMismatch { expected, actual })) => {
                assert_eq!(
                    expected, remaining,
                    "the range names the rest of the entity"
                );
                assert_eq!(actual, remaining + 1, "the body ran one byte long");
            }
            other => panic!("a body longer than its range is refused: {other:?}"),
        }
        assert_eq!(
            file_length(&part).await,
            0,
            "a partial file past its range cannot be resumed"
        );
        assert!(!exists(&destination).await);
    }

    #[tokio::test]
    async fn a_chunked_partial_response_that_fills_its_range_completes() {
        let payload = payload(128 * 1024);
        let server = Arc::new(Server::new(payload.clone()).with_chunked_range(ChunkedRange::Exact));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 100 * 1024;
        tokio::fs::write(&part, &payload[..prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let request = request(address, "/payload", &destination, None);

        let written = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await
        .expect("an unannounced body that fills its range completes");

        assert_eq!(written, payload.len() as u64);
        assert_eq!(file_content(&destination).await, payload.to_vec());
        assert!(!exists(&part).await);
    }

    #[tokio::test]
    async fn a_chunked_partial_response_past_its_class_budget_is_refused() {
        let server = Arc::new(Server::new(payload(1024)));
        let address = spawn(Arc::clone(&server)).await;
        let (_directory, destination, part) = downloading();
        let prefix = 512;
        tokio::fs::write(&part, vec![0xEE_u8; prefix])
            .await
            .expect("the partial file is written");
        let hub = transport();
        let config = Config::default();
        let runtime = test_handle();
        let mut request = request(address, "/oversized-range", &destination, None);
        request.class = HttpClientClass::UpdaterDownload;

        let outcome = download(
            &hub,
            &config,
            &runtime,
            &request,
            false,
            validator(),
            &CancellationToken::new(),
            &Progress::default(),
        )
        .await;

        match outcome {
            Err(DownloadError::TooLarge { limit }) => assert_eq!(
                limit,
                HttpClientClass::UpdaterDownload.response_body_limit() as u64,
                "the refusal names the budget of the download's class"
            ),
            other => panic!("a partial response past the class budget is refused: {other:?}"),
        }
        assert!(
            !exists(&part).await,
            "an oversized download leaves no partial file to resume"
        );
        assert!(!exists(&destination).await);
    }

    #[test]
    fn a_content_range_should_describe_the_bytes_it_names() {
        assert_eq!(
            content_range("bytes 0-99/1000"),
            Some(ContentRange {
                start: 0,
                end: 99,
                total: Some(1000),
            })
        );
        assert_eq!(
            content_range("bytes 10-19/*"),
            Some(ContentRange {
                start: 10,
                end: 19,
                total: None,
            }),
            "the total may be unknown while the range is not"
        );
    }

    #[test]
    fn a_content_range_should_accept_any_case_of_its_unit() {
        let expected = Some(ContentRange {
            start: 0,
            end: 99,
            total: Some(1000),
        });

        assert_eq!(content_range("Bytes 0-99/1000"), expected);
        assert_eq!(content_range("BYTES 0-99/1000"), expected);
        assert_eq!(content_range("byte 0-99/1000"), None);
        assert_eq!(content_range("items 0-99/1000"), None);
    }

    #[test]
    fn a_content_range_should_reject_a_malformed_value() {
        assert_eq!(content_range("bytes 0/1000"), None, "no end");
        assert_eq!(content_range("bytes 0-99"), None, "no total");
        assert_eq!(content_range("bytes a-99/1000"), None, "no number");
        assert_eq!(
            content_range("bytes */1000"),
            None,
            "the unsatisfied form names no bytes"
        );
    }

    #[test]
    fn a_content_range_should_reject_an_end_before_or_past_the_entity() {
        assert_eq!(
            content_range("bytes 99-0/1000"),
            None,
            "an end before start"
        );
        assert_eq!(
            content_range("bytes 0-1000/1000"),
            None,
            "an end at the total is one byte past the entity"
        );
        assert_eq!(
            content_range("bytes 0-999/1000").map(|range| range.end),
            Some(999),
            "the last byte of the entity is in range"
        );
    }

    #[test]
    fn a_redirect_target_should_refuse_leaving_https() {
        let current = Url::parse("https://example.com/release").expect("the fixture is a URL");

        assert!(
            matches!(
                redirect_target(&current, Some("http://example.com/release"), 0, 10),
                Err(DownloadError::Redirect(RedirectFailure::Downgrade))
            ),
            "a plaintext hop from an encrypted one is refused"
        );
        assert!(
            redirect_target(&current, Some("https://example.com/release"), 0, 10).is_ok(),
            "an encrypted hop is followed"
        );
    }

    #[test]
    fn a_redirect_target_should_stop_at_the_hop_limit() {
        let current = Url::parse("https://example.com/release").expect("the fixture is a URL");

        assert!(
            matches!(
                redirect_target(&current, Some("/next"), 10, 10),
                Err(DownloadError::Redirect(RedirectFailure::HopLimit))
            ),
            "a hop budget that is spent is not exceeded"
        );
        assert!(
            redirect_target(&current, Some("/next"), 9, 10).is_ok(),
            "the last hop of the budget is followed"
        );
    }
}

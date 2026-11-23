use std::{
    io,
    path::Path,
    sync::{Arc, Mutex, PoisonError, Weak},
    time::{Duration, Instant},
};

use ahash::AHashMap;
use app_config::network::Config;
use app_config_provider::notification::ConfigNotification;
use network::transport::hub::TransportHub;
use tokio::{
    runtime::Handle,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{
    error::{DownloadError, DownloadFailure},
    fetch::{self, Validator},
    progress::{Progress, Snapshot},
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadStatus, DownloadTask},
};

pub(crate) const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);
const ATTEMPT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const FILE_REMOVAL_TIMEOUT: Duration = Duration::from_secs(10);

pub struct DownloadProviderDeps {
    pub transport: Arc<TransportHub>,
    pub runtime: Handle,
}

struct Entry {
    request: DownloadRequest,
    status: DownloadStatus,
    validator: Arc<Mutex<Option<Validator>>>,
    fresh: bool,
    attempt: Option<Attempt>,
}

struct Attempt {
    cancellation: CancellationToken,
    progress: Arc<Progress>,
    tasks: Vec<JoinHandle<()>>,
}

struct State {
    entries: AHashMap<DownloadId, Entry>,
    order: Vec<DownloadId>,
    sink: Option<Arc<dyn Fn(DownloadEvent) + Send + Sync>>,
}

pub struct DownloadProvider {
    inner: Arc<Inner>,
}

struct Inner {
    transport: Arc<TransportHub>,
    runtime: Handle,
    config: ConfigNotification<Config>,
    state: Mutex<State>,
    delete_tasks: Mutex<Vec<JoinHandle<()>>>,
}

pub struct DownloadSubscription {
    inner: Weak<Inner>,
    sink: Option<Arc<dyn Fn(DownloadEvent) + Send + Sync>>,
}

impl DownloadSubscription {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            inner: Weak::new(),
            sink: None,
        }
    }
}

impl Drop for DownloadSubscription {
    fn drop(&mut self) {
        let Some(sink) = self.sink.take() else {
            return;
        };
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .sink
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &sink))
        {
            state.sink = None;
        }
    }
}

impl std::fmt::Debug for DownloadProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DownloadProvider")
            .field("tasks", &self.tasks().len())
            .finish()
    }
}

impl DownloadProvider {
    #[must_use]
    pub fn new(deps: DownloadProviderDeps, config: ConfigNotification<Config>) -> Self {
        Self {
            inner: Arc::new(Inner {
                transport: deps.transport,
                runtime: deps.runtime,
                config,
                state: Mutex::new(State {
                    entries: AHashMap::with_capacity(4),
                    order: Vec::with_capacity(4),
                    sink: None,
                }),
                delete_tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn add(&self, id: DownloadId, request: DownloadRequest) {
        self.inner.add(id, request);
    }

    #[must_use]
    pub fn tasks(&self) -> Vec<DownloadTask> {
        self.inner.tasks()
    }

    #[must_use]
    pub fn task(&self, id: DownloadId) -> Option<DownloadTask> {
        self.inner.task(id)
    }

    pub fn pause(&self, id: DownloadId) -> bool {
        self.inner.pause(id)
    }

    pub fn resume(&self, id: DownloadId) -> bool {
        self.inner.resume(id)
    }

    pub fn delete(&self, id: DownloadId) -> bool {
        self.inner.delete(id)
    }

    pub fn subscribe(
        &self,
        sink: Arc<dyn Fn(DownloadEvent) + Send + Sync>,
    ) -> DownloadSubscription {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.sink = Some(Arc::clone(&sink));
        drop(state);

        DownloadSubscription {
            inner: Arc::downgrade(&self.inner),
            sink: Some(sink),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.sink = None;
        for entry in state.entries.values() {
            if let Some(attempt) = &entry.attempt {
                attempt.cancellation.cancel();
            }
        }
    }
}

impl Inner {
    fn add(self: &Arc<Self>, id: DownloadId, request: DownloadRequest) {
        let status = DownloadStatus::Downloading {
            downloaded: 0,
            total: request.expected_size,
            mb_per_second: 0.0,
        };
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.entries.insert(
                id,
                Entry {
                    request,
                    status,
                    validator: Arc::new(Mutex::new(None)),
                    fresh: true,
                    attempt: None,
                },
            );
            state.order.push(id);
        }
        self.start_attempt(id, true);
        self.publish(id);
    }

    fn tasks(&self) -> Vec<DownloadTask> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut tasks = Vec::with_capacity(state.order.len());
        for id in &state.order {
            if let Some(entry) = state.entries.get(id) {
                tasks.push(task_snapshot(*id, entry));
            }
        }

        tasks
    }

    fn task(&self, id: DownloadId) -> Option<DownloadTask> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.entries.get(&id).map(|entry| task_snapshot(id, entry))
    }

    fn pause(&self, id: DownloadId) -> bool {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = state.entries.get_mut(&id) else {
                return false;
            };
            let (downloaded, total) = match &entry.status {
                DownloadStatus::Downloading {
                    downloaded, total, ..
                } => (*downloaded, *total),
                _ => return false,
            };
            match &entry.attempt {
                Some(attempt) => {
                    attempt.cancellation.cancel();
                    entry.status = DownloadStatus::Pausing { downloaded, total };
                }
                None => entry.status = DownloadStatus::Paused { downloaded, total },
            }
        }
        self.publish(id);

        true
    }

    fn resume(self: &Arc<Self>, id: DownloadId) -> bool {
        let fresh = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = state.entries.get_mut(&id) else {
                return false;
            };
            let total = match &entry.status {
                DownloadStatus::Paused { total, .. } => *total,
                DownloadStatus::Failed { .. } => entry.request.expected_size,
                _ => return false,
            };
            let fresh = entry.fresh;
            let downloaded = if fresh {
                0
            } else {
                part_length(&entry.request.destination)
            };
            entry.status = DownloadStatus::Downloading {
                downloaded,
                total,
                mb_per_second: 0.0,
            };
            fresh
        };
        self.start_attempt(id, fresh);
        self.publish(id);

        true
    }

    fn delete(self: &Arc<Self>, id: DownloadId) -> bool {
        let (request, pending) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = state.entries.get_mut(&id) else {
                return false;
            };
            if matches!(entry.status, DownloadStatus::Deleting) {
                return false;
            }
            entry.status = DownloadStatus::Deleting;
            let attempt = entry.attempt.take();
            if let Some(attempt) = &attempt {
                attempt.cancellation.cancel();
            }
            (
                entry.request.clone(),
                attempt.map(|attempt| attempt.tasks).unwrap_or_default(),
            )
        };
        self.publish(id);

        let inner = Arc::downgrade(self);
        let runtime = self.runtime.clone();
        let delete = self.runtime.spawn(async move {
            let outcome = if wait_for_attempts(pending).await {
                remove_download_files(&runtime, &request.destination).await
            } else {
                log::warn!(
                    "the download to {:?} did not stop before its files were deleted",
                    request.destination
                );
                Err(DownloadFailure::Io {
                    kind: io::ErrorKind::TimedOut,
                })
            };
            if let Err(error) = &outcome {
                log::warn!(
                    "failed to delete the download to {:?}: {error:?}",
                    request.destination,
                );
            }
            if let Some(inner) = inner.upgrade() {
                inner.finish_delete(id, outcome);
            }
        });
        let mut delete_tasks = self
            .delete_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        delete_tasks.retain(|task| !task.is_finished());
        delete_tasks.push(delete);

        true
    }

    fn start_attempt(self: &Arc<Self>, id: DownloadId, fresh: bool) {
        let attempt = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = state.entries.get_mut(&id) else {
                return;
            };
            let DownloadStatus::Downloading {
                downloaded: starting_from,
                ..
            } = &entry.status
            else {
                return;
            };
            let starting_from = *starting_from;
            if let Some(previous) = entry.attempt.take() {
                previous.cancellation.cancel();
                for task in &previous.tasks {
                    task.abort();
                }
            }
            let cancellation = self.transport.cancellation().child_token();
            let progress = Arc::new(Progress::default());
            entry.attempt = Some(Attempt {
                cancellation: cancellation.clone(),
                progress: Arc::clone(&progress),
                tasks: Vec::with_capacity(2),
            });
            (
                entry.request.clone(),
                Arc::clone(&entry.validator),
                starting_from,
                cancellation,
                progress,
            )
        };
        let (request, validator, starting_from, cancellation, progress) = attempt;

        let transport = Arc::clone(&self.transport);
        let config = self.config.current();
        let resolving = self.runtime.clone();
        let transfer_cancellation = cancellation.clone();
        let transfer_progress = Arc::clone(&progress);
        let downloading = self.runtime.spawn(async move {
            fetch::download(
                &transport,
                &config,
                &resolving,
                &request,
                fresh,
                validator,
                &transfer_cancellation,
                &transfer_progress,
            )
            .await
        });

        let publishing_inner = Arc::downgrade(self);
        let publishing_cancellation = cancellation;
        let publishing_progress = Arc::clone(&progress);
        let publishing = self.runtime.spawn(async move {
            let mut sampled_at = Instant::now();
            let mut sampled = starting_from;
            loop {
                tokio::select! {
                    biased;
                    () = publishing_cancellation.cancelled() => return,
                    () = sleep(PUBLISH_INTERVAL) => {}
                }
                let snapshot = publishing_progress.snapshot();
                let now = Instant::now();
                let interval = now.saturating_duration_since(sampled_at);
                let rate = snapshot.megabytes_per_second_since(sampled, interval);
                sampled_at = now;
                sampled = snapshot.written();
                let Some(inner) = publishing_inner.upgrade() else {
                    return;
                };
                if !inner.publish_progress(id, snapshot, rate) {
                    return;
                }
            }
        });

        let outcome_inner = Arc::downgrade(self);
        let outcome = self.runtime.spawn(async move {
            let result = match downloading.await {
                Ok(result) => result,
                Err(error) => Err(DownloadError::Task(error)),
            };
            if let Some(inner) = outcome_inner.upgrade() {
                inner.finish_attempt(id, result);
            }
        });

        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(attempt) = state
            .entries
            .get_mut(&id)
            .and_then(|entry| entry.attempt.as_mut())
        {
            attempt.tasks = vec![publishing, outcome];
        }
    }

    fn publish_progress(&self, id: DownloadId, snapshot: Snapshot, rate: f32) -> bool {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = state.entries.get_mut(&id) else {
                return false;
            };
            let DownloadStatus::Downloading {
                downloaded,
                total,
                mb_per_second,
            } = &mut entry.status
            else {
                return false;
            };
            *downloaded = snapshot.written();
            if let Some(named) = snapshot.total() {
                *total = Some(named);
            }
            *mb_per_second = rate;
        }
        self.publish(id);

        true
    }

    fn finish_attempt(&self, id: DownloadId, result: Result<u64, DownloadError>) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = state.entries.get_mut(&id) else {
                return;
            };
            if matches!(entry.status, DownloadStatus::Deleting) {
                return;
            }
            if entry
                .attempt
                .as_ref()
                .is_some_and(|attempt| attempt.progress.snapshot().opened())
            {
                entry.fresh = false;
            }
            entry.status = match result {
                Ok(total) => DownloadStatus::Completed {
                    downloaded: total,
                    total,
                },
                Err(error) => match &entry.status {
                    DownloadStatus::Pausing { total, .. } => DownloadStatus::Paused {
                        downloaded: if entry.fresh {
                            0
                        } else {
                            part_length(&entry.request.destination)
                        },
                        total: *total,
                    },
                    _ => DownloadStatus::Failed {
                        failure: error.failure(),
                    },
                },
            };
        }
        self.publish(id);
    }

    fn finish_delete(&self, id: DownloadId, result: Result<(), DownloadFailure>) {
        if let Err(failure) = result {
            {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(entry) = state.entries.get_mut(&id) {
                    entry.status = DownloadStatus::Failed { failure };
                }
            }
            self.publish(id);
            return;
        }
        let removed = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let removed = state.entries.remove(&id).is_some();
            state.order.retain(|other| *other != id);
            removed
        };
        if removed {
            self.emit(DownloadEvent::TaskRemoved(id));
        }
    }

    fn publish(&self, id: DownloadId) {
        if let Some(task) = self.task(id) {
            self.emit(DownloadEvent::TaskUpdated(task));
        }
    }

    fn emit(&self, event: DownloadEvent) {
        let sink = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.sink.clone()
        };
        if let Some(sink) = sink {
            sink(event);
        }
    }

    #[cfg(test)]
    fn insert_pausing_for_test(
        &self,
        id: DownloadId,
        request: DownloadRequest,
        fresh: bool,
        opened: bool,
    ) {
        let progress = Progress::default();
        if opened {
            progress.opened_at(part_length(&request.destination), None);
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.entries.insert(
            id,
            Entry {
                request,
                status: DownloadStatus::Pausing {
                    downloaded: 0,
                    total: None,
                },
                validator: Arc::new(Mutex::new(None)),
                fresh,
                attempt: Some(Attempt {
                    cancellation: CancellationToken::new(),
                    progress: Arc::new(progress),
                    tasks: Vec::new(),
                }),
            },
        );
        state.order.push(id);
    }

    #[cfg(test)]
    fn fresh_for_test(&self, id: DownloadId) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.entries.get(&id).is_some_and(|entry| entry.fresh)
    }
}

fn task_snapshot(id: DownloadId, entry: &Entry) -> DownloadTask {
    DownloadTask {
        id,
        url: entry.request.url.clone(),
        destination: entry.request.destination.clone(),
        status: entry.status.clone(),
    }
}

fn part_length(destination: &Path) -> u64 {
    crate::partial::path(destination)
        .and_then(|part| std::fs::metadata(part).ok())
        .map_or(0, |metadata| metadata.len())
}

async fn wait_for_attempts(pending: Vec<JoinHandle<()>>) -> bool {
    let joined = async move {
        for task in pending {
            let _ = task.await;
        }
    };

    timeout(ATTEMPT_STOP_TIMEOUT, joined).await.is_ok()
}

async fn remove_download_files(
    runtime: &Handle,
    destination: &Path,
) -> Result<(), DownloadFailure> {
    let Some(part) = crate::partial::path(destination) else {
        return Ok(());
    };
    let files = vec![part, destination.to_path_buf()];
    let removal = runtime.spawn_blocking(move || {
        let mut failure = None;
        for path in &files {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != io::ErrorKind::NotFound
                && failure.is_none()
            {
                failure = Some(error.kind());
            }
        }
        failure
    });

    match timeout(FILE_REMOVAL_TIMEOUT, removal).await {
        Ok(Ok(Some(kind))) => Err(DownloadFailure::Io { kind }),
        Ok(Ok(None)) => Ok(()),
        Ok(Err(error)) => {
            log::warn!("the download's files could not be removed: {error}");
            Err(DownloadFailure::Io {
                kind: io::ErrorKind::Other,
            })
        }
        Err(_) => Err(DownloadFailure::Io {
            kind: io::ErrorKind::TimedOut,
        }),
    }
}
#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        net::{Ipv4Addr, SocketAddr},
        path::Path,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
        time::{Duration, Instant},
    };

    use app_config::network::{Config, RouteTarget, ServiceTarget};
    use app_config_provider::notification::ConfigNotification;
    use hyper::{
        Request, Response, StatusCode,
        body::{Body, Bytes, Frame, Incoming, SizeHint},
        header::{CONTENT_LENGTH, CONTENT_RANGE},
        server::conn::http1,
        service::service_fn,
    };
    use hyper_util::rt::TokioIo;
    use network::transport::{
        http::HttpClientClass,
        hub::{TransportHub, TransportPolicy},
    };
    use tempfile::TempDir;
    use tokio::{net::TcpListener, time::Sleep};
    use url::Url;
    use uuid::Uuid;

    use super::{DownloadProvider, DownloadProviderDeps};
    use crate::{
        error::DownloadFailure,
        request::DownloadRequest,
        task::{DownloadEvent, DownloadId, DownloadStatus},
        test_support::test_handle,
    };

    const FRAME: usize = 16 * 1024;
    const PACE: Duration = Duration::from_millis(25);
    const WAIT_LIMIT: Duration = Duration::from_secs(10);

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
    }

    struct Server {
        payload: Bytes,
        pace: Option<Duration>,
        requests: Mutex<Vec<Recorded>>,
    }

    impl Server {
        fn whole(payload: Bytes) -> Self {
            Self {
                payload,
                pace: None,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn paced(payload: Bytes) -> Self {
            Self {
                payload,
                pace: Some(PACE),
                requests: Mutex::new(Vec::new()),
            }
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
            self.requests
                .lock()
                .expect("the request log is never poisoned")
                .push(Recorded {
                    path: path.clone(),
                    range: range.clone(),
                });

            if path != "/payload" {
                return self.reply(
                    StatusCode::NOT_FOUND,
                    None,
                    TestBody::whole(Bytes::new()),
                    0,
                );
            }

            self.payload_response(range.as_deref())
        }

        fn payload_response(&self, range: Option<&str>) -> Response<TestBody> {
            let length = self.payload.len() as u64;
            let Some(start) = range.and_then(requested_start) else {
                return self.reply(
                    StatusCode::OK,
                    None,
                    self.body(self.payload.clone()),
                    length,
                );
            };
            if start >= length {
                return self.reply(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    Some(format!("bytes */{length}")),
                    TestBody::whole(Bytes::new()),
                    0,
                );
            }
            let remaining = length - start;
            self.reply(
                StatusCode::PARTIAL_CONTENT,
                Some(format!("bytes {start}-{}/{length}", length - 1)),
                self.body(self.payload.slice(start as usize..)),
                remaining,
            )
        }

        fn body(&self, bytes: Bytes) -> TestBody {
            match self.pace {
                Some(pace) => TestBody::paced(bytes, pace),
                None => TestBody::whole(bytes),
            }
        }

        fn reply(
            &self,
            status: StatusCode,
            content_range: Option<String>,
            body: TestBody,
            length: u64,
        ) -> Response<TestBody> {
            let mut builder = Response::builder()
                .status(status)
                .header(CONTENT_LENGTH, length);
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

    fn header(request: &Request<Incoming>, name: &str) -> Option<String> {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    fn spawn_server(server: Arc<Server>) -> SocketAddr {
        let runtime = test_handle();
        let listener = runtime
            .block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .expect("a loopback port is free");
        let address = listener.local_addr().expect("the listener has an address");
        drop(runtime.spawn(async move {
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
        }));

        address
    }

    fn url(address: SocketAddr, path: &str) -> Url {
        Url::parse(&format!("http://{address}{path}")).expect("the request URL is a URL")
    }

    fn spawn_stalled_server() -> SocketAddr {
        let runtime = test_handle();
        let listener = runtime
            .block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .expect("a loopback port is free");
        let address = listener.local_addr().expect("the listener has an address");
        drop(runtime.spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    drop(stream);
                });
            }
        }));

        address
    }

    fn request(
        address: SocketAddr,
        path: &str,
        destination: &Path,
        expected_size: Option<u64>,
    ) -> DownloadRequest {
        DownloadRequest {
            url: url(address, path),
            destination: destination.to_path_buf(),
            target: RouteTarget::Service(ServiceTarget::TranslationModelDownload),
            class: HttpClientClass::ModelDownload,
            expected_size,
        }
    }

    fn provider() -> DownloadProvider {
        DownloadProvider::new(
            DownloadProviderDeps {
                transport: Arc::new(TransportHub::new(TransportPolicy {
                    allow_plaintext: true,
                    extra_root_certificates: Vec::new(),
                })),
                runtime: test_handle(),
            },
            ConfigNotification::new(Arc::new(Config::default())),
        )
    }

    struct EventLog {
        recorded: Arc<Mutex<Vec<DownloadEvent>>>,
        _subscription: super::DownloadSubscription,
    }

    impl EventLog {
        fn recorded(&self) -> Vec<DownloadEvent> {
            self.recorded
                .lock()
                .expect("the event log is never poisoned")
                .clone()
        }

        fn published(&self, id: DownloadId) -> Vec<DownloadStatus> {
            self.recorded()
                .into_iter()
                .filter_map(|event| match event {
                    DownloadEvent::TaskUpdated(task) if task.id == id => Some(task.status),
                    _ => None,
                })
                .collect()
        }
    }

    fn events(provider: &DownloadProvider) -> EventLog {
        let recorded: Arc<Mutex<Vec<DownloadEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        let subscription = provider.subscribe(Arc::new(move |event: DownloadEvent| {
            sink.lock()
                .expect("the event log is never poisoned")
                .push(event);
        }));

        EventLog {
            recorded,
            _subscription: subscription,
        }
    }

    fn status(provider: &DownloadProvider, id: DownloadId) -> Option<DownloadStatus> {
        provider.task(id).map(|task| task.status)
    }

    fn add(provider: &DownloadProvider, request: DownloadRequest) -> DownloadId {
        let id = DownloadId::new();
        provider.add(id, request);
        id
    }

    fn pause(provider: &DownloadProvider, id: DownloadId) -> bool {
        provider.pause(id)
    }

    fn resume(provider: &DownloadProvider, id: DownloadId) -> bool {
        provider.resume(id)
    }

    fn delete(provider: &DownloadProvider, id: DownloadId) -> bool {
        provider.delete(id)
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            if condition() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn adding_a_download_should_create_a_unique_task_and_publish_it() {
        let payload = payload(192 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider();
        let events = events(&provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let first_destination = directory.path().join("first.bin");
        let second_destination = directory.path().join("second.bin");
        let expected = Some(payload.len() as u64);

        let first = add(
            &provider,
            request(address, "/payload", &first_destination, expected),
        );
        let second = add(
            &provider,
            request(address, "/payload", &second_destination, expected),
        );

        assert_ne!(first, second, "every added download gets its own identity");
        assert_eq!(
            Uuid::parse_str(&first.to_string()).expect("the identity prints a UUID"),
            first.as_uuid(),
        );
        assert_eq!(
            Uuid::parse_str(&second.to_string()).expect("the identity prints a UUID"),
            second.as_uuid(),
        );

        let tasks = provider.tasks();
        assert_eq!(
            tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
            [first, second],
            "tasks answer in the order the caller added them",
        );
        let task = provider.task(first).expect("the added task is held");
        assert_eq!(task.id, first);
        assert_eq!(task.destination, first_destination);
        assert_eq!(task.url, url(address, "/payload"));
        assert_eq!(
            task.status,
            DownloadStatus::Downloading {
                downloaded: 0,
                total: expected,
                mb_per_second: 0.0,
            },
        );

        let published = events.recorded();
        let added = published
            .iter()
            .find_map(|event| match event {
                DownloadEvent::TaskUpdated(task) if task.id == first => Some(task.clone()),
                _ => None,
            })
            .expect("the added task is published");
        assert_eq!(added, task, "the event carries the task snapshot");
        assert!(
            published.iter().any(|event| matches!(
                event,
                DownloadEvent::TaskUpdated(task) if task.id == second
            )),
            "the second task is published under its own identity",
        );
    }

    #[test]
    fn a_destination_without_a_file_name_should_fail_instead_of_panicking() {
        let provider = provider();
        let unreachable = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        let id = add(
            &provider,
            request(unreachable, "/payload", Path::new(""), None),
        );

        wait_until("the invalid destination to fail", || {
            matches!(
                status(&provider, id),
                Some(DownloadStatus::Failed {
                    failure: DownloadFailure::InvalidDestination,
                })
            )
        });
        assert!(
            delete(&provider, id),
            "a task that never had a file can still be removed",
        );
        wait_until("the invalid task to be removed", || {
            provider.tasks().is_empty()
        });
    }

    #[test]
    fn a_download_should_reach_its_destination_and_remove_the_part() {
        let payload = payload(128 * 1024);
        let address = spawn_server(Arc::new(Server::whole(payload.clone())));
        let provider = provider();
        let events = events(&provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until("the download to complete", || {
            status(&provider, id)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
        });

        assert_eq!(
            std::fs::read(&destination).expect("the destination file is readable"),
            payload.to_vec(),
        );
        assert!(!part.exists(), "the partial file is renamed into place");

        let statuses = events.published(id);
        assert!(
            matches!(statuses.first(), Some(DownloadStatus::Downloading { .. })),
            "the task is published as downloading when it is added: {statuses:?}",
        );
        assert_eq!(
            statuses.last(),
            Some(&DownloadStatus::Completed {
                downloaded: expected,
                total: expected,
            }),
            "the last state the task publishes is its completion",
        );
    }

    #[test]
    fn pausing_a_download_should_keep_the_part_and_resuming_should_ask_for_it() {
        let payload = payload(2 * 1024 * 1024);
        let server = Arc::new(Server::paced(payload.clone()));
        let address = spawn_server(Arc::clone(&server));
        let provider = provider();
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until("some bytes to arrive before the pause", || {
            matches!(
                status(&provider, id),
                Some(DownloadStatus::Downloading { downloaded, .. }) if downloaded > 0
            )
        });

        assert!(pause(&provider, id), "a running download can be paused");
        let pausing = match status(&provider, id) {
            Some(DownloadStatus::Pausing { downloaded, total }) => {
                assert_eq!(total, Some(expected));
                downloaded
            }
            other => panic!("the pausing task should carry its progress: {other:?}"),
        };
        assert!(
            pausing > 0,
            "the pause starts from the progress the task already had",
        );
        wait_until("the download to settle as paused", || {
            matches!(status(&provider, id), Some(DownloadStatus::Paused { .. }))
        });
        let paused_at = match status(&provider, id) {
            Some(DownloadStatus::Paused { downloaded, total }) => {
                assert_eq!(total, Some(expected));
                downloaded
            }
            other => panic!("the paused task should carry its progress: {other:?}"),
        };
        assert!(
            paused_at > 0 && paused_at < expected,
            "the pause keeps the bytes that arrived: {paused_at}",
        );
        assert_eq!(
            std::fs::metadata(&part)
                .expect("the partial file survives the pause")
                .len(),
            paused_at,
            "the paused status is the partial file the resume continues from",
        );
        assert!(
            !destination.exists(),
            "a paused download is not a destination file",
        );

        assert!(resume(&provider, id), "a paused download can be resumed");
        wait_until("the resumed download to complete", || {
            status(&provider, id)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
        });

        assert_eq!(
            std::fs::read(&destination).expect("the destination file is readable"),
            payload.to_vec(),
        );
        assert!(!part.exists(), "the resumed transfer renames the part");

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            2,
            "the pause and the resume make two requests"
        );
        assert_eq!(requests[0].path, "/payload");
        assert_eq!(
            requests[0].range, None,
            "the first request asks for the whole entity",
        );
        let expected_range = format!("bytes={paused_at}-");
        assert_eq!(
            requests[1].range.as_deref(),
            Some(expected_range.as_str()),
            "the resume starts where the pause left the partial file",
        );
    }

    #[test]
    fn deleting_a_completed_download_should_remove_the_task_and_its_files() {
        let payload = payload(64 * 1024);
        let address = spawn_server(Arc::new(Server::whole(payload.clone())));
        let provider = provider();
        let events = events(&provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until("the download to complete", || {
            status(&provider, id)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
        });

        assert!(delete(&provider, id), "a completed task can be deleted");
        assert_eq!(
            status(&provider, id),
            Some(DownloadStatus::Deleting),
            "the delete publishes its own state",
        );
        assert!(
            !delete(&provider, id),
            "a task that is already being deleted is refused",
        );

        wait_until("the task to be removed", || provider.tasks().is_empty());

        assert!(!destination.exists(), "the destination is removed");
        assert!(!part.exists(), "the partial file is removed");
        let published = events.recorded();
        assert!(
            published.iter().any(
                |event| matches!(event, DownloadEvent::TaskRemoved(removed) if *removed == id)
            ),
            "the removal is published",
        );
    }

    #[test]
    fn deleting_a_download_in_flight_should_cancel_it_and_remove_the_part() {
        let payload = payload(2 * 1024 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider();
        let events = events(&provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until("some bytes to arrive before the delete", || {
            matches!(
                status(&provider, id),
                Some(DownloadStatus::Downloading { downloaded, .. }) if downloaded > 0
            )
        });
        assert!(part.exists(), "the transfer has written a partial file");

        assert!(delete(&provider, id), "a download in flight can be deleted",);
        wait_until("the task to be removed", || provider.tasks().is_empty());

        assert!(
            !part.exists(),
            "the partial file of a cancelled download is removed",
        );
        assert!(!destination.exists(), "nothing was renamed into place");

        let published = events.recorded();
        assert!(
            published.iter().any(|event| matches!(
                event,
                DownloadEvent::TaskUpdated(task)
                    if task.id == id && task.status == DownloadStatus::Deleting
            )),
            "the delete publishes the deleting state",
        );
        let removed = published
            .iter()
            .position(
                |event| matches!(event, DownloadEvent::TaskRemoved(removed) if *removed == id),
            )
            .expect("the removal is published");
        assert!(
            !published[removed + 1..].iter().any(|event| matches!(
                event,
                DownloadEvent::TaskUpdated(task) if task.id == id
            )),
            "a removed task publishes nothing after its removal",
        );
    }

    #[test]
    fn two_downloads_should_publish_only_their_own_tasks() {
        let payload = payload(160 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider();
        let events = events(&provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let first_destination = directory.path().join("first.bin");
        let second_destination = directory.path().join("second.bin");
        let expected = payload.len() as u64;

        let first = add(
            &provider,
            request(address, "/payload", &first_destination, Some(expected)),
        );
        let second = add(
            &provider,
            request(address, "/payload", &second_destination, Some(expected)),
        );
        wait_until("both downloads to complete", || {
            status(&provider, first)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
                && status(&provider, second)
                    == Some(DownloadStatus::Completed {
                        downloaded: expected,
                        total: expected,
                    })
        });

        assert_eq!(
            std::fs::read(&first_destination).expect("the first destination is readable"),
            payload.to_vec(),
        );
        assert_eq!(
            std::fs::read(&second_destination).expect("the second destination is readable"),
            payload.to_vec(),
        );

        let published = events.recorded();
        let mut first_updates = 0_usize;
        let mut second_updates = 0_usize;
        for event in &published {
            let DownloadEvent::TaskUpdated(task) = event else {
                continue;
            };
            assert!(
                task.id == first || task.id == second,
                "a task published here belongs to one of the two downloads",
            );
            if task.id == first {
                assert_eq!(
                    task.destination, first_destination,
                    "the first download's events name its own destination",
                );
                first_updates += 1;
            } else {
                assert_eq!(
                    task.destination, second_destination,
                    "the second download's events name its own destination",
                );
                second_updates += 1;
            }
        }
        assert!(
            first_updates > 0,
            "the first download published its progress"
        );
        assert!(
            second_updates > 0,
            "the second download published its progress",
        );
    }

    #[test]
    fn progress_should_be_published_once_an_interval_even_while_nothing_arrives() {
        let address = spawn_stalled_server();
        let provider = provider();
        let events = events(&provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");

        let id = add(
            &provider,
            request(address, "/payload", &destination, Some(4096)),
        );
        assert_eq!(
            events.published(id).len(),
            1,
            "adding publishes the starting state",
        );

        for interval in 1_usize..=3 {
            wait_until("the next interval publish", || {
                events.published(id).len() > interval
            });
            let statuses = events.published(id);
            assert_eq!(
                statuses.len(),
                interval + 1,
                "the download publishes once an interval, whether or not bytes arrive",
            );
            assert!(
                matches!(
                    statuses.last(),
                    Some(DownloadStatus::Downloading {
                        downloaded: 0,
                        mb_per_second,
                        ..
                    }) if *mb_per_second == 0.0
                ),
                "a transfer that wrote nothing reports no rate: {statuses:?}",
            );
        }
    }

    #[test]
    fn a_download_should_publish_the_rate_it_writes_at() {
        let payload = payload(2 * 1024 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider();
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");

        let id = add(
            &provider,
            request(
                address,
                "/payload",
                &destination,
                Some(payload.len() as u64),
            ),
        );
        wait_until("a rate the transfer is writing at", || {
            matches!(
                status(&provider, id),
                Some(DownloadStatus::Downloading { mb_per_second, .. })
                    if mb_per_second > 0.0 && mb_per_second < 100.0
            )
        });
    }

    #[test]
    fn a_pause_should_settle_at_what_the_partial_file_holds_once_it_was_opened() {
        use crate::error::DownloadError;

        let provider = provider();
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let partial = 4 * 1024_u64;
        std::fs::write(&part, vec![0xEE_u8; partial as usize])
            .expect("the partial file is written");

        let insert_pausing = |fresh: bool, opened: bool| -> DownloadId {
            let id = DownloadId::new();
            provider.inner.insert_pausing_for_test(
                id,
                request(
                    SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
                    "/payload",
                    &destination,
                    None,
                ),
                fresh,
                opened,
            );
            id
        };
        let settle = |id: DownloadId| {
            provider
                .inner
                .finish_attempt(id, Err(DownloadError::Cancelled));
        };

        let unopened = insert_pausing(true, false);
        settle(unopened);
        assert_eq!(
            status(&provider, unopened),
            Some(DownloadStatus::Paused {
                downloaded: 0,
                total: None,
            }),
            "a task whose transfer never opened the partial file resumes from zero",
        );
        assert!(
            provider.inner.fresh_for_test(unopened),
            "the next attempt of that task still starts from zero",
        );

        let opened = insert_pausing(true, true);
        settle(opened);
        assert_eq!(
            status(&provider, opened),
            Some(DownloadStatus::Paused {
                downloaded: partial,
                total: None,
            }),
            "a task whose transfer opened the partial file resumes from the bytes it wrote",
        );
        assert!(
            !provider.inner.fresh_for_test(opened),
            "the bytes on disk are the next attempt's resume point",
        );

        let resumed = insert_pausing(false, false);
        settle(resumed);
        assert_eq!(
            status(&provider, resumed),
            Some(DownloadStatus::Paused {
                downloaded: partial,
                total: None,
            }),
            "a task that already opened the file keeps its resume point",
        );
        assert!(!provider.inner.fresh_for_test(resumed));
    }

    #[test]
    fn a_pause_should_be_refused_unless_the_download_is_running() {
        let payload = payload(2 * 1024 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider();
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let expected = payload.len() as u64;

        let id = add(
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until("the download to report progress", || {
            matches!(
                status(&provider, id),
                Some(DownloadStatus::Downloading { downloaded, .. }) if downloaded > 0
            )
        });

        assert!(pause(&provider, id));
        assert!(
            !pause(&provider, id),
            "a download that is already pausing has nothing to pause",
        );
        assert!(
            !resume(&provider, id),
            "a download that is still pausing cannot be resumed",
        );

        wait_until("the download to settle as paused", || {
            matches!(status(&provider, id), Some(DownloadStatus::Paused { .. }))
        });
        assert!(
            !pause(&provider, id),
            "a paused download has nothing to pause",
        );
        assert!(resume(&provider, id), "a paused download can be resumed");

        wait_until("the resumed download to complete", || {
            status(&provider, id)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
        });
        assert!(
            !pause(&provider, id),
            "a completed download has nothing to pause",
        );
        assert!(
            !resume(&provider, id),
            "a completed download has nothing to resume",
        );
    }

    #[test]
    fn resuming_a_failed_download_should_start_another_attempt() {
        let address = spawn_server(Arc::new(Server::whole(payload(1024))));
        let provider = provider();
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");

        let id = add(&provider, request(address, "/missing", &destination, None));
        wait_until("the download to fail", || {
            matches!(
                status(&provider, id),
                Some(DownloadStatus::Failed {
                    failure: DownloadFailure::Status { status: 404 },
                })
            )
        });

        assert!(
            resume(&provider, id),
            "a failed download is retried like a paused one",
        );
        assert!(
            matches!(
                status(&provider, id),
                Some(DownloadStatus::Downloading { .. })
            ),
            "the retry starts another attempt",
        );
    }

    #[test]
    fn unknown_ids_should_be_refused() {
        let provider = provider();
        let unknown = DownloadId::new();

        assert!(!pause(&provider, unknown));
        assert!(!resume(&provider, unknown));
        assert!(!delete(&provider, unknown));
        assert_eq!(provider.task(unknown), None);
        assert!(provider.tasks().is_empty());
    }

    #[test]
    fn adding_a_download_should_discard_a_partial_file_it_did_not_write() {
        let payload = payload(96 * 1024);
        let server = Arc::new(Server::whole(payload.clone()));
        let address = spawn_server(Arc::clone(&server));
        let provider = provider();
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let prefix = 32 * 1024;
        std::fs::write(&part, vec![0xEE_u8; prefix]).expect("the stale partial file is written");
        let expected = payload.len() as u64;

        let id = add(
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until("the download to complete", || {
            status(&provider, id)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
        });

        assert_eq!(
            std::fs::read(&destination).expect("the destination file is readable"),
            payload.to_vec(),
        );
        assert!(!part.exists(), "the partial file is renamed into place");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].range, None,
            "a new task never continues a partial file it did not write",
        );
    }

    #[test]
    fn a_dropped_subscription_should_stop_the_events() {
        let provider = provider();
        let recorded: Arc<Mutex<Vec<DownloadEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        let subscription = provider.subscribe(Arc::new(move |event| {
            sink.lock()
                .expect("the event log is never poisoned")
                .push(event);
        }));
        drop(subscription);

        let directory = TempDir::new().expect("a temporary directory is available");
        let id = add(
            &provider,
            request(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
                "/payload",
                &directory.path().join("model.bin"),
                None,
            ),
        );
        wait_until("the unobserved download to fail", || {
            matches!(status(&provider, id), Some(DownloadStatus::Failed { .. }))
        });
        assert!(
            recorded
                .lock()
                .expect("the event log is never poisoned")
                .is_empty(),
            "a dropped subscription receives no events",
        );
    }
}

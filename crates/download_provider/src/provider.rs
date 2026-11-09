//! The download manager entity: the tasks it owns, the states they settle in,
//! and the events that publish them.
//!
//! A task is one transfer, started on the application runtime and reported back
//! to this entity by two interface tasks — one for progress, one for the outcome
//! — which the entry holds for as long as the attempt lasts. Pausing cancels the
//! attempt and settles at the partial file once it has stopped; deleting cancels
//! it too, and only removes the files once both tasks have finished.

use std::{
    io,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ahash::AHashMap;
use app_config::network::Config;
use app_config_provider::notification::ConfigNotification;
use gpui_kit::{BackgroundExecutor, Context, EventEmitter, Task};
use network::transport::hub::TransportHub;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use widget_tokio_runtime::runtime::Runtime;

use crate::{
    error::{DownloadError, DownloadFailure},
    fetch::{self, Validator},
    global::DownloadProviderDeps,
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadStatus, DownloadTask},
};

/// How often a delete task checks whether the work it waits for has stopped.
const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long a delete waits for the cancelled attempt to settle.
const ATTEMPT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a delete waits for its file removal to finish.
const FILE_REMOVAL_TIMEOUT: Duration = Duration::from_secs(10);

/// One download as the manager holds it.
struct Entry {
    /// What the caller asked for; every attempt is started from this.
    request: DownloadRequest,
    /// What observers currently see.
    status: DownloadStatus,
    /// The entity validator a resume is conditional on, shared with the transfer.
    validator: Arc<Mutex<Option<Validator>>>,
    /// Whether the next attempt still has to start from zero.
    ///
    /// A task is fresh from the moment it is added until its transfer first
    /// reports progress. That report is sent only after the attempt has opened
    /// and truncated the partial file, so from then on the bytes on disk are
    /// this task's own resume point. A pause before that report leaves the flag
    /// set, and the next resume truncates again instead of continuing a partial
    /// file this task never wrote.
    fresh: bool,
    /// The transfer in flight, or the last one when nothing is running.
    ///
    /// The tasks are what keep the attempt running: a dropped interface task is
    /// a cancelled one, so the entry owns them until the next attempt replaces
    /// them or a delete moves them into its own task.
    attempt: Option<Attempt>,
}

/// One started transfer: its cancellation boundary and the tasks that report it.
struct Attempt {
    cancellation: CancellationToken,
    tasks: Vec<Task<()>>,
}

/// The download manager: every task in this process, and their states.
pub struct DownloadProvider {
    transport: Arc<TransportHub>,
    runtime: Runtime,
    config: ConfigNotification<Config>,
    entries: AHashMap<DownloadId, Entry>,
    /// The insertion order `tasks` answers in.
    order: Vec<DownloadId>,
    /// The delete tasks that have not finished yet.
    ///
    /// A delete task belongs here rather than in the entry it deletes: the task
    /// is what removes that entry, so holding it in the entry would drop it —
    /// and cancel it — while it is doing the removing. A finished task is
    /// dropped when the next delete starts, so a long session does not keep one
    /// task per delete it ever ran.
    delete_tasks: Vec<Task<()>>,
}

impl EventEmitter<DownloadEvent> for DownloadProvider {}

impl DownloadProvider {
    /// The manager over the composition root's dependencies.
    pub(crate) fn new(deps: DownloadProviderDeps, config: ConfigNotification<Config>) -> Self {
        Self {
            transport: deps.transport,
            runtime: deps.runtime,
            config,
            entries: AHashMap::with_capacity(4),
            order: Vec::with_capacity(4),
            delete_tasks: Vec::new(),
        }
    }

    /// Adds a download and starts it immediately.
    ///
    /// A new task never continues a partial file: its first attempt starts from
    /// zero and truncates any `.part` it finds, so bytes that no attempt of this
    /// task wrote cannot be stitched onto the download. The partial file is the
    /// resume point of this task's own pause and resume only.
    ///
    /// The same holds for a resume before the first attempt has reported any
    /// progress: the transfer reports only once it has opened the partial file,
    /// so until then a resume starts from zero too, and pausing before the file
    /// was ever opened cannot inherit bytes this task did not write.
    ///
    /// The destination is not validated here: a destination with no file name
    /// fails its first attempt with [`DownloadFailure::InvalidDestination`] rather
    /// than panicking the caller.
    pub fn add(&mut self, request: DownloadRequest, cx: &mut Context<Self>) -> DownloadId {
        let id = DownloadId::new();
        let status = DownloadStatus::Downloading {
            downloaded: 0,
            total: request.expected_size,
        };
        self.entries.insert(
            id,
            Entry {
                request,
                status,
                validator: Arc::new(Mutex::new(None)),
                fresh: true,
                attempt: None,
            },
        );
        self.order.push(id);
        self.start_attempt(id, true, cx);
        self.publish(id, cx);

        id
    }

    /// Every task, in the order the caller added them.
    ///
    /// A task stays here after it completes or fails; only [`Self::delete`]
    /// removes it.
    pub fn tasks(&self) -> Vec<DownloadTask> {
        let mut tasks = Vec::with_capacity(self.order.len());
        for id in &self.order {
            if let Some(task) = self.task(*id) {
                tasks.push(task);
            }
        }

        tasks
    }

    /// The task `id` names, when it is still held.
    pub fn task(&self, id: DownloadId) -> Option<DownloadTask> {
        self.entries.get(&id).map(|entry| DownloadTask {
            id,
            url: entry.request.url.clone(),
            destination: entry.request.destination.clone(),
            status: entry.status.clone(),
        })
    }

    /// Stops a transfer that is in flight, keeping its partial file for a resume.
    ///
    /// The task is `Pausing` until the transfer has stopped and settled as
    /// [`DownloadStatus::Paused`]. Any other state refuses the pause, because
    /// there is nothing running that could be stopped.
    pub fn pause(&mut self, id: DownloadId, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        let (downloaded, total) = match &entry.status {
            DownloadStatus::Downloading { downloaded, total } => (*downloaded, *total),
            _ => return false,
        };
        if let Some(attempt) = &entry.attempt {
            // The attempt stays owned: cancelling only asks it to stop, and its
            // outcome task is what settles the state as `Paused`.
            attempt.cancellation.cancel();
            entry.status = DownloadStatus::Pausing { downloaded, total };
        } else {
            entry.status = DownloadStatus::Paused { downloaded, total };
        }
        self.publish(id, cx);

        true
    }

    /// Starts a transfer again from its partial file.
    ///
    /// Accepted from `Paused` and from `Failed`: a failed transfer is retried the
    /// same way a paused one is continued, from the bytes the partial file holds.
    ///
    /// A task that has not reported progress yet starts from zero even here: its
    /// first attempt never got as far as opening the partial file, so a resume
    /// keeps the fresh transfer's guarantee rather than continuing a file this
    /// task did not write.
    pub fn resume(&mut self, id: DownloadId, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
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
        entry.status = DownloadStatus::Downloading { downloaded, total };
        self.start_attempt(id, fresh, cx);
        self.publish(id, cx);

        true
    }

    /// Removes a task and the files it owns.
    ///
    /// A running transfer is cancelled first, and its files are only removed once
    /// the attempt has stopped writing them. Deleting the same task twice is
    /// refused while the first delete is still in flight.
    pub fn delete(&mut self, id: DownloadId, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        if matches!(entry.status, DownloadStatus::Deleting) {
            return false;
        }
        entry.status = DownloadStatus::Deleting;
        let request = entry.request.clone();
        let attempt = entry.attempt.take();
        if let Some(attempt) = &attempt {
            attempt.cancellation.cancel();
        }
        self.publish(id, cx);

        let runtime = self.runtime.clone();
        let pending = attempt.map(|attempt| attempt.tasks).unwrap_or_default();
        let delete = cx.spawn(async move |provider, cx| {
            let clock = cx.background_executor().clone();
            let outcome = if wait_for_attempts(pending, &clock).await {
                remove_download_files(&runtime, &request.destination, &clock).await
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
            let _ = provider.update(cx, |provider, cx| provider.finish_delete(id, outcome, cx));
        });
        // Only a delete that is still running needs its task held.
        self.delete_tasks.retain(|task| !task.is_ready());
        self.delete_tasks.push(delete);

        true
    }

    /// Starts one transfer for `id`, replacing whatever attempt the entry held.
    ///
    /// `fresh` is what the transfer does with an existing partial file: a new
    /// task's first attempt truncates it, while a resumed task continues it.
    fn start_attempt(&mut self, id: DownloadId, fresh: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(&id) else {
            return;
        };
        let request = entry.request.clone();
        let validator = Arc::clone(&entry.validator);
        let transport = Arc::clone(&self.transport);
        let config = self.config.current();
        let runtime = self.runtime.clone();
        let resolving = runtime.clone();
        let cancellation = self.transport.cancellation().child_token();
        let download_cancellation = cancellation.clone();
        let (progress_sender, mut progress) = mpsc::unbounded_channel();

        let downloading = runtime.run(async move {
            fetch::download(
                &transport,
                &config,
                &resolving,
                &request,
                fresh,
                validator,
                &download_cancellation,
                move |written, total| {
                    // This runs on the task reading the response, so it only hands
                    // the numbers over; the entity is updated on the interface
                    // thread by the progress task below.
                    let _ = progress_sender.send((written, total));
                },
            )
            .await
        });
        let progress_task = cx.spawn(async move |provider, cx| {
            while let Some((written, total)) = progress.recv().await {
                let _ = provider.update(cx, |provider, cx| {
                    provider.record_progress(id, written, total, cx)
                });
            }
        });
        let outcome_task = cx.spawn(async move |provider, cx| {
            let result = match downloading.await {
                Ok(result) => result,
                Err(error) => Err(DownloadError::Task(error)),
            };
            let _ = provider.update(cx, |provider, cx| provider.finish_attempt(id, result, cx));
        });
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.attempt = Some(Attempt {
                cancellation,
                tasks: vec![progress_task, outcome_task],
            });
        }
    }

    /// Records the bytes a transfer has written; `total` is `None` while the
    /// response has not named one.
    fn record_progress(
        &mut self,
        id: DownloadId,
        written: u64,
        total: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        let DownloadStatus::Downloading {
            downloaded,
            total: known,
        } = &mut entry.status
        else {
            return;
        };
        // A transfer reports only after it has opened the partial file, so this
        // report is what makes the bytes on disk this task's own resume point.
        // It is cleared before the numbers are compared: a first report that
        // repeats the seeded zero is still proof that the file was opened.
        entry.fresh = false;
        // A number below the last one is a server that answered `200` after a
        // range request, which restarts the file at zero; it is progress, not a
        // stale report.
        if *downloaded == written && *known == total {
            return;
        }
        *downloaded = written;
        *known = total;
        self.publish(id, cx);
    }

    /// Applies what an attempt produced: the final size, a cancellation that the
    /// pause asked for, or a failure.
    fn finish_attempt(
        &mut self,
        id: DownloadId,
        result: Result<u64, DownloadError>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        // A delete cancelled this attempt; the delete task owns the files and the
        // state from here on, so the outcome only has to stop being a download.
        if matches!(entry.status, DownloadStatus::Deleting) {
            return;
        }
        entry.status = match result {
            Ok(total) => DownloadStatus::Completed {
                downloaded: total,
                total,
            },
            Err(error) => match &entry.status {
                DownloadStatus::Pausing { total, .. } => DownloadStatus::Paused {
                    // A task that never reported progress has not opened its
                    // partial file yet; its resume truncates the file and starts
                    // from zero, so the paused state announces that same starting
                    // point instead of the length of a file this task never wrote.
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
        self.publish(id, cx);
    }

    /// Applies what a delete produced: a removed task, or a failure the user can
    /// retry the delete from.
    fn finish_delete(
        &mut self,
        id: DownloadId,
        result: Result<(), DownloadFailure>,
        cx: &mut Context<Self>,
    ) {
        if let Err(failure) = result {
            if let Some(entry) = self.entries.get_mut(&id) {
                entry.status = DownloadStatus::Failed { failure };
            }
            self.publish(id, cx);
            return;
        }
        self.entries.remove(&id);
        self.order.retain(|other| *other != id);
        cx.emit(DownloadEvent::TaskRemoved(id));
        cx.notify();
    }

    /// Publishes the snapshot observers react to.
    fn publish(&mut self, id: DownloadId, cx: &mut Context<Self>) {
        let Some(task) = self.task(id) else {
            return;
        };
        cx.emit(DownloadEvent::TaskUpdated(task));
        cx.notify();
    }

    /// The network projection this manager routes with, for a test that installs
    /// it without a composition root.
    #[cfg(test)]
    pub(crate) fn config_for_test(&self) -> &ConfigNotification<Config> {
        &self.config
    }
}

/// The bytes a partial file already holds, `0` when there is none to continue.
fn part_length(destination: &Path) -> u64 {
    crate::partial::path(destination)
        .and_then(|part| std::fs::metadata(part).ok())
        .map_or(0, |metadata| metadata.len())
}

/// Waits for a cancelled attempt's tasks to finish.
///
/// The interface clock is what this task may be woken by, and the deadline is
/// what keeps an attempt that never stops from pinning the task in `Deleting`.
async fn wait_for_attempts(pending: Vec<Task<()>>, clock: &BackgroundExecutor) -> bool {
    let deadline = clock.now() + ATTEMPT_STOP_TIMEOUT;
    while !pending.iter().all(Task::is_ready) {
        if clock.now() >= deadline {
            return false;
        }
        clock.timer(DELETE_POLL_INTERVAL).await;
    }

    true
}

/// Removes the partial file and the destination a task owns.
///
/// The removal runs on the runtime's blocking pool. A file that is already gone
/// is already deleted; the first real failure is reported. When the destination
/// has no file name this task could not have created a file there, so there is
/// nothing to remove.
async fn remove_download_files(
    runtime: &Runtime,
    destination: &Path,
    clock: &BackgroundExecutor,
) -> Result<(), DownloadFailure> {
    let Some(part) = crate::partial::path(destination) else {
        return Ok(());
    };
    let files = vec![part, destination.to_path_buf()];
    let completion: Arc<Mutex<Option<Option<io::ErrorKind>>>> = Arc::new(Mutex::new(None));
    let worker_completion = Arc::clone(&completion);
    drop(runtime.blocking(move || {
        let mut failure = None;
        for path in &files {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != io::ErrorKind::NotFound
                && failure.is_none()
            {
                failure = Some(error.kind());
            }
        }
        *worker_completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(failure);
    }));

    // Poll the completion rather than awaiting the blocking handle: the result is
    // delivered from a worker thread, and the interface executor is what this
    // task may be woken by.
    let deadline = clock.now() + FILE_REMOVAL_TIMEOUT;
    loop {
        if let Some(failure) = completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return match failure {
                Some(kind) => Err(DownloadFailure::Io { kind }),
                None => Ok(()),
            };
        }
        if clock.now() >= deadline {
            return Err(DownloadFailure::Io {
                kind: io::ErrorKind::TimedOut,
            });
        }
        clock.timer(DELETE_POLL_INTERVAL).await;
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
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
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
    use widget_tokio_runtime::runtime::Runtime;

    use super::{DownloadProvider, Entry};
    use crate::{
        error::{DownloadError, DownloadFailure},
        global::DownloadProviderDeps,
        request::DownloadRequest,
        task::{DownloadEvent, DownloadId, DownloadStatus},
    };

    /// How much of a paced payload each frame carries.
    const FRAME: usize = 16 * 1024;
    /// How long a paced server waits between frames.
    const PACE: Duration = Duration::from_millis(25);
    /// How long a test waits for an asynchronous outcome before failing.
    const WAIT_LIMIT: Duration = Duration::from_secs(10);

    /// The bytes a test downloads, different at every offset so a misplaced
    /// resume is visible.
    fn payload(length: usize) -> Bytes {
        Bytes::from(
            (0..length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<u8>>(),
        )
    }

    /// A response body the test server hands out one frame at a time.
    ///
    /// The pause between frames is what makes a transfer observably in flight;
    /// without it the whole payload reaches the kernel at once and there is
    /// nothing left to interrupt.
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

    /// One request the test server answered, reduced to what a download sets.
    #[derive(Clone, Debug)]
    struct Recorded {
        path: String,
        range: Option<String>,
    }

    /// The state the test server answers from.
    struct Server {
        payload: Bytes,
        pace: Option<Duration>,
        requests: Mutex<Vec<Recorded>>,
    }

    impl Server {
        /// A server that sends the whole entity as fast as the connection takes it.
        fn whole(payload: Bytes) -> Self {
            Self {
                payload,
                pace: None,
                requests: Mutex::new(Vec::new()),
            }
        }

        /// A server that sends the whole entity one paced frame at a time.
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

    /// The first byte a `Range` request asks for.
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

    /// Starts a server answering `server` on a loopback port.
    ///
    /// The accept loop runs on the shared test runtime for the rest of the
    /// process; a test only needs the address.
    fn spawn_server(server: Arc<Server>) -> SocketAddr {
        let runtime = Runtime::for_test();
        let listener = runtime
            .handle()
            .block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .expect("a loopback port is free");
        let address = listener.local_addr().expect("the listener has an address");
        drop(runtime.run(async move {
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

    /// The provider over a plaintext transport, without a composition root.
    fn provider(cx: &mut TestAppContext) -> Entity<DownloadProvider> {
        // The transfers report back from the runtime's threads, which the
        // deterministic scheduler would otherwise refuse as non-determinism.
        // This test drives real HTTP, so it parks like an integration harness.
        cx.executor().allow_parking();
        cx.update(|cx| {
            cx.new(|_| {
                DownloadProvider::new(
                    DownloadProviderDeps {
                        transport: Arc::new(TransportHub::new(TransportPolicy {
                            allow_plaintext: true,
                            extra_root_certificates: Vec::new(),
                        })),
                        runtime: Runtime::for_test(),
                    },
                    ConfigNotification::new(Arc::new(Config::default())),
                )
            })
        })
    }

    /// Subscribes `provider`'s events into a log the test asserts on.
    fn events(
        cx: &mut TestAppContext,
        provider: &Entity<DownloadProvider>,
    ) -> Arc<Mutex<Vec<DownloadEvent>>> {
        let recorded: Arc<Mutex<Vec<DownloadEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        cx.update(|cx| {
            cx.subscribe(provider, move |_, event: &DownloadEvent, _| {
                sink.lock()
                    .expect("the event log is never poisoned")
                    .push(event.clone());
            })
            .detach();
        });

        recorded
    }

    fn recorded(events: &Arc<Mutex<Vec<DownloadEvent>>>) -> Vec<DownloadEvent> {
        events
            .lock()
            .expect("the event log is never poisoned")
            .clone()
    }

    fn status(
        cx: &TestAppContext,
        provider: &Entity<DownloadProvider>,
        id: DownloadId,
    ) -> Option<DownloadStatus> {
        cx.update(|cx| provider.read(cx).task(id).map(|task| task.status))
    }

    fn add(
        cx: &mut TestAppContext,
        provider: &Entity<DownloadProvider>,
        request: DownloadRequest,
    ) -> DownloadId {
        cx.update(|cx| provider.update(cx, |provider, cx| provider.add(request, cx)))
    }

    fn pause(cx: &mut TestAppContext, provider: &Entity<DownloadProvider>, id: DownloadId) -> bool {
        cx.update(|cx| provider.update(cx, |provider, cx| provider.pause(id, cx)))
    }

    fn resume(
        cx: &mut TestAppContext,
        provider: &Entity<DownloadProvider>,
        id: DownloadId,
    ) -> bool {
        cx.update(|cx| provider.update(cx, |provider, cx| provider.resume(id, cx)))
    }

    fn delete(
        cx: &mut TestAppContext,
        provider: &Entity<DownloadProvider>,
        id: DownloadId,
    ) -> bool {
        cx.update(|cx| provider.update(cx, |provider, cx| provider.delete(id, cx)))
    }

    /// Polls `condition` on the interface executor until it holds, with real time
    /// passing so the transfer it observes can make progress.
    fn wait_until(
        cx: &mut TestAppContext,
        what: &str,
        mut condition: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            if condition(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[gpui_kit::test]
    fn adding_a_download_should_create_a_unique_task_and_publish_it(cx: &mut TestAppContext) {
        let payload = payload(192 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider(cx);
        let events = events(cx, &provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let first_destination = directory.path().join("first.bin");
        let second_destination = directory.path().join("second.bin");
        let expected = Some(payload.len() as u64);

        let first = add(
            cx,
            &provider,
            request(address, "/payload", &first_destination, expected),
        );
        let second = add(
            cx,
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

        let tasks = cx.update(|cx| provider.read(cx).tasks());
        assert_eq!(
            tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
            [first, second],
            "tasks answer in the order the caller added them",
        );
        let task = cx
            .update(|cx| provider.read(cx).task(first))
            .expect("the added task is held");
        assert_eq!(task.id, first);
        assert_eq!(task.destination, first_destination);
        assert_eq!(task.url, url(address, "/payload"));
        assert_eq!(
            task.status,
            DownloadStatus::Downloading {
                downloaded: 0,
                total: expected,
            },
        );

        let published = recorded(&events);
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

    /// A destination that could not name a file is a failed task, never a panic.
    #[gpui_kit::test]
    fn a_destination_without_a_file_name_should_fail_instead_of_panicking(cx: &mut TestAppContext) {
        let provider = provider(cx);
        let unreachable = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        let id = add(
            cx,
            &provider,
            request(unreachable, "/payload", Path::new(""), None),
        );

        wait_until(cx, "the invalid destination to fail", |cx| {
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Failed {
                    failure: DownloadFailure::InvalidDestination,
                })
            )
        });
        assert!(
            delete(cx, &provider, id),
            "a task that never had a file can still be removed",
        );
        wait_until(cx, "the invalid task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });
    }

    #[gpui_kit::test]
    fn a_download_should_reach_its_destination_and_remove_the_part(cx: &mut TestAppContext) {
        let payload = payload(128 * 1024);
        let address = spawn_server(Arc::new(Server::whole(payload.clone())));
        let provider = provider(cx);
        let events = events(cx, &provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            cx,
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until(cx, "the download to complete", |cx| {
            status(cx, &provider, id)
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

        let published = recorded(&events);
        let statuses = published
            .iter()
            .filter_map(|event| match event {
                DownloadEvent::TaskUpdated(task) if task.id == id => Some(task.status.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
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

    #[gpui_kit::test]
    fn pausing_a_download_should_keep_the_part_and_resuming_should_ask_for_it(
        cx: &mut TestAppContext,
    ) {
        let payload = payload(192 * 1024);
        let server = Arc::new(Server::paced(payload.clone()));
        let address = spawn_server(Arc::clone(&server));
        let provider = provider(cx);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            cx,
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until(cx, "some bytes to arrive before the pause", |cx| {
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Downloading { downloaded, .. }) if downloaded > 0
            )
        });

        assert!(pause(cx, &provider, id), "a running download can be paused");
        let pausing = match status(cx, &provider, id) {
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
        wait_until(cx, "the download to settle as paused", |cx| {
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Paused { .. })
            )
        });
        let paused_at = match status(cx, &provider, id) {
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

        assert!(
            resume(cx, &provider, id),
            "a paused download can be resumed"
        );
        wait_until(cx, "the resumed download to complete", |cx| {
            status(cx, &provider, id)
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

    #[gpui_kit::test]
    fn deleting_a_completed_download_should_remove_the_task_and_its_files(cx: &mut TestAppContext) {
        let payload = payload(64 * 1024);
        let address = spawn_server(Arc::new(Server::whole(payload.clone())));
        let provider = provider(cx);
        let events = events(cx, &provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            cx,
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until(cx, "the download to complete", |cx| {
            status(cx, &provider, id)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
        });

        assert!(delete(cx, &provider, id), "a completed task can be deleted");
        assert_eq!(
            status(cx, &provider, id),
            Some(DownloadStatus::Deleting),
            "the delete publishes its own state",
        );
        assert!(
            !delete(cx, &provider, id),
            "a task that is already being deleted is refused",
        );

        wait_until(cx, "the task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });

        assert!(!destination.exists(), "the destination is removed");
        assert!(!part.exists(), "the partial file is removed");
        let published = recorded(&events);
        assert!(
            published.iter().any(
                |event| matches!(event, DownloadEvent::TaskRemoved(removed) if *removed == id)
            ),
            "the removal is published",
        );
    }

    #[gpui_kit::test]
    fn deleting_a_download_in_flight_should_cancel_it_and_remove_the_part(cx: &mut TestAppContext) {
        let payload = payload(192 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider(cx);
        let events = events(cx, &provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let expected = payload.len() as u64;

        let id = add(
            cx,
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until(cx, "some bytes to arrive before the delete", |cx| {
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Downloading { downloaded, .. }) if downloaded > 0
            )
        });
        assert!(part.exists(), "the transfer has written a partial file");

        assert!(
            delete(cx, &provider, id),
            "a download in flight can be deleted",
        );
        wait_until(cx, "the task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });

        assert!(
            !part.exists(),
            "the partial file of a cancelled download is removed",
        );
        assert!(!destination.exists(), "nothing was renamed into place");

        let published = recorded(&events);
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

    #[gpui_kit::test]
    fn two_downloads_should_publish_only_their_own_tasks(cx: &mut TestAppContext) {
        let payload = payload(160 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider(cx);
        let events = events(cx, &provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let first_destination = directory.path().join("first.bin");
        let second_destination = directory.path().join("second.bin");
        let expected = payload.len() as u64;

        let first = add(
            cx,
            &provider,
            request(address, "/payload", &first_destination, Some(expected)),
        );
        let second = add(
            cx,
            &provider,
            request(address, "/payload", &second_destination, Some(expected)),
        );
        wait_until(cx, "both downloads to complete", |cx| {
            status(cx, &provider, first)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
                && status(cx, &provider, second)
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

        let published = recorded(&events);
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

    /// A repeated progress report is not news, while a report that goes back to
    /// zero is: it is the server restarting the entity it would not range.
    #[gpui_kit::test]
    fn progress_should_be_published_only_when_it_changes(cx: &mut TestAppContext) {
        let provider = provider(cx);
        let events = events(cx, &provider);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let id = DownloadId::new();
        cx.update(|cx| {
            provider.update(cx, |provider, _| {
                provider.entries.insert(
                    id,
                    Entry {
                        request: request(
                            SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
                            "/payload",
                            &destination,
                            None,
                        ),
                        status: DownloadStatus::Downloading {
                            downloaded: 0,
                            total: None,
                        },
                        validator: Arc::new(Mutex::new(None)),
                        fresh: false,
                        attempt: None,
                    },
                );
                provider.order.push(id);
            });
        });

        let updates = || {
            recorded(&events)
                .into_iter()
                .filter(|event| matches!(event, DownloadEvent::TaskUpdated(task) if task.id == id))
                .count()
        };
        let record = |cx: &mut TestAppContext, written: u64, total: Option<u64>| {
            cx.update(|cx| {
                provider.update(cx, |provider, cx| {
                    provider.record_progress(id, written, total, cx)
                })
            });
        };

        record(cx, 0, None);
        assert_eq!(updates(), 0, "the seeded status is not published again");

        record(cx, 32, Some(100));
        assert_eq!(updates(), 1, "changed progress is published");

        record(cx, 32, Some(100));
        assert_eq!(updates(), 1, "the same numbers are not published again");

        record(cx, 0, Some(100));
        assert_eq!(
            updates(),
            2,
            "a server that restarts the entity reports zero",
        );
        assert_eq!(
            status(cx, &provider, id),
            Some(DownloadStatus::Downloading {
                downloaded: 0,
                total: Some(100),
            }),
        );
    }

    /// A fresh task stays fresh until its transfer reports progress: a pause
    /// before that report means the partial file was never opened, so even a
    /// resume still has to begin from zero.
    #[gpui_kit::test]
    fn a_task_should_stay_fresh_until_its_transfer_reports_progress(cx: &mut TestAppContext) {
        let provider = provider(cx);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let id = DownloadId::new();
        cx.update(|cx| {
            provider.update(cx, |provider, _| {
                provider.entries.insert(
                    id,
                    Entry {
                        request: request(
                            SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
                            "/payload",
                            &destination,
                            None,
                        ),
                        status: DownloadStatus::Pausing {
                            downloaded: 0,
                            total: None,
                        },
                        validator: Arc::new(Mutex::new(None)),
                        fresh: true,
                        attempt: None,
                    },
                );
                provider.order.push(id);
            });
        });

        let fresh = |cx: &TestAppContext| {
            cx.update(|cx| {
                provider
                    .read(cx)
                    .entries
                    .get(&id)
                    .expect("the entry is held")
                    .fresh
            })
        };
        let record = |cx: &mut TestAppContext, written: u64, total: Option<u64>| {
            cx.update(|cx| {
                provider.update(cx, |provider, cx| {
                    provider.record_progress(id, written, total, cx)
                })
            });
        };

        // A transfer that is being stopped never reported, so the partial file
        // it would have opened is not this task's resume point.
        record(cx, 0, None);
        assert!(
            fresh(cx),
            "a report outside a running transfer does not clear fresh"
        );

        cx.update(|cx| {
            provider.update(cx, |provider, _| {
                if let Some(entry) = provider.entries.get_mut(&id) {
                    entry.status = DownloadStatus::Downloading {
                        downloaded: 0,
                        total: None,
                    };
                }
            });
        });
        // The first report repeats the seeded zero and is still the proof that
        // the transfer opened (and truncated) the partial file.
        record(cx, 0, None);
        assert!(
            !fresh(cx),
            "the first running report makes the partial file resumable"
        );
    }

    /// A paused task that never reported progress truncates on resume, so its
    /// `Paused` state must announce the same starting point rather than the
    /// length of a partial file it did not write.
    #[gpui_kit::test]
    fn a_pause_before_the_first_report_should_show_zero_progress(cx: &mut TestAppContext) {
        let provider = provider(cx);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let partial = 4 * 1024_u64;
        std::fs::write(&part, vec![0xEE_u8; partial as usize])
            .expect("the partial file is written");

        let insert_pausing = |cx: &mut TestAppContext, fresh: bool| -> DownloadId {
            let id = DownloadId::new();
            cx.update(|cx| {
                provider.update(cx, |provider, _| {
                    provider.entries.insert(
                        id,
                        Entry {
                            request: request(
                                SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
                                "/payload",
                                &destination,
                                None,
                            ),
                            status: DownloadStatus::Pausing {
                                downloaded: 0,
                                total: None,
                            },
                            validator: Arc::new(Mutex::new(None)),
                            fresh,
                            attempt: None,
                        },
                    );
                    provider.order.push(id);
                });
            });

            id
        };
        let settle = |cx: &mut TestAppContext, id: DownloadId| {
            cx.update(|cx| {
                provider.update(cx, |provider, cx| {
                    provider.finish_attempt(id, Err(DownloadError::Cancelled), cx)
                });
            });
        };

        let fresh = insert_pausing(cx, true);
        settle(cx, fresh);
        assert_eq!(
            status(cx, &provider, fresh),
            Some(DownloadStatus::Paused {
                downloaded: 0,
                total: None,
            }),
            "a fresh task that never reported progress resumes from zero",
        );

        let resumable = insert_pausing(cx, false);
        settle(cx, resumable);
        assert_eq!(
            status(cx, &provider, resumable),
            Some(DownloadStatus::Paused {
                downloaded: partial,
                total: None,
            }),
            "a task that reported progress resumes from the partial file it wrote",
        );
    }

    #[gpui_kit::test]
    fn a_pause_should_be_refused_unless_the_download_is_running(cx: &mut TestAppContext) {
        let payload = payload(192 * 1024);
        let address = spawn_server(Arc::new(Server::paced(payload.clone())));
        let provider = provider(cx);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let expected = payload.len() as u64;

        let id = add(
            cx,
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until(cx, "the download to report progress", |cx| {
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Downloading { downloaded, .. }) if downloaded > 0
            )
        });

        assert!(pause(cx, &provider, id));
        assert!(
            !pause(cx, &provider, id),
            "a download that is already pausing has nothing to pause",
        );
        assert!(
            !resume(cx, &provider, id),
            "a download that is still pausing cannot be resumed",
        );

        wait_until(cx, "the download to settle as paused", |cx| {
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Paused { .. })
            )
        });
        assert!(
            !pause(cx, &provider, id),
            "a paused download has nothing to pause",
        );
        assert!(
            resume(cx, &provider, id),
            "a paused download can be resumed"
        );

        wait_until(cx, "the resumed download to complete", |cx| {
            status(cx, &provider, id)
                == Some(DownloadStatus::Completed {
                    downloaded: expected,
                    total: expected,
                })
        });
        assert!(
            !pause(cx, &provider, id),
            "a completed download has nothing to pause",
        );
        assert!(
            !resume(cx, &provider, id),
            "a completed download has nothing to resume",
        );
    }

    #[gpui_kit::test]
    fn resuming_a_failed_download_should_start_another_attempt(cx: &mut TestAppContext) {
        let address = spawn_server(Arc::new(Server::whole(payload(1024))));
        let provider = provider(cx);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");

        let id = add(
            cx,
            &provider,
            request(address, "/missing", &destination, None),
        );
        wait_until(cx, "the download to fail", |cx| {
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Failed {
                    failure: DownloadFailure::Status { status: 404 },
                })
            )
        });

        assert!(
            resume(cx, &provider, id),
            "a failed download is retried like a paused one",
        );
        assert!(
            matches!(
                status(cx, &provider, id),
                Some(DownloadStatus::Downloading { .. })
            ),
            "the retry starts another attempt",
        );
    }

    #[gpui_kit::test]
    fn unknown_ids_should_be_refused(cx: &mut TestAppContext) {
        let provider = provider(cx);
        let unknown = DownloadId::new();

        assert!(!pause(cx, &provider, unknown));
        assert!(!resume(cx, &provider, unknown));
        assert!(!delete(cx, &provider, unknown));
        assert_eq!(cx.update(|cx| provider.read(cx).task(unknown)), None);
        assert!(cx.update(|cx| provider.read(cx).tasks().is_empty()));
    }

    /// A new task starts from zero, whatever partial file it finds at its
    /// destination: nothing else in this process wrote it, so it is not resumed.
    #[gpui_kit::test]
    fn adding_a_download_should_discard_a_partial_file_it_did_not_write(cx: &mut TestAppContext) {
        let payload = payload(96 * 1024);
        let server = Arc::new(Server::whole(payload.clone()));
        let address = spawn_server(Arc::clone(&server));
        let provider = provider(cx);
        let directory = TempDir::new().expect("a temporary directory is available");
        let destination = directory.path().join("model.bin");
        let part = directory.path().join("model.bin.part");
        let prefix = 32 * 1024;
        std::fs::write(&part, vec![0xEE_u8; prefix]).expect("the stale partial file is written");
        let expected = payload.len() as u64;

        let id = add(
            cx,
            &provider,
            request(address, "/payload", &destination, Some(expected)),
        );
        wait_until(cx, "the download to complete", |cx| {
            status(cx, &provider, id)
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
}

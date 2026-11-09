//! The process-wide translation model downloads.
//!
//! A download belongs to the process rather than to a settings session: closing the settings
//! page must not cancel a model that is still arriving, and opening the page again must show
//! the progress the download has reached. The entity is therefore installed as a global by the
//! composition root, and the settings page only observes the entity it finds there.
//!
//! Each download streams into `{file_name}.part` beside its destination and is renamed into
//! place only after its exact published size has arrived. The state the page draws lives here
//! too, so a failure or a delete that could not be made is still there when the page is opened
//! again.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use ahash::AHashMap;
use app_config::{
    network::{Config, ConfigError, RouteTarget, ServiceTarget},
    translation::TranslationModel,
};
use app_config_provider::manager::ConfigManager;
use gpui_kit::{
    App, AppContext as _, BackgroundExecutor, BorrowAppContext as _, Context, Entity, EventEmitter,
    Global, Task,
};
use network::{
    proxy::resolve_async_cancellable,
    transport::{error::TransportError, http::HttpClientClass, hub::TransportHub},
};
use reqwest::{
    Response, StatusCode,
    header::{ACCEPT_ENCODING, LOCATION},
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{fs::File, io::AsyncWriteExt, sync::mpsc};
use tokio_util::sync::CancellationToken;
use url::{ParseError, Url};
use widget_tokio_runtime::runtime::Runtime;

/// How many new bytes may accumulate before progress is reported.
const PROGRESS_REPORT_BYTES: u64 = 1024 * 1024;
/// How much wall-clock time may pass before progress is reported.
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// How often a delete task checks whether its blocking worker has finished.
const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long a delete waits for a cancelled download of the same model to stop.
const PENDING_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a delete may stay in `Deleting` before it is reported as failed.
///
/// The engine's own unload is bounded at 30 seconds; this leaves the file removal after it.
const DELETE_TIMEOUT: Duration = Duration::from_secs(35);

/// Why the translation engine could not release a model file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnloadFailure {
    /// Translations kept the model in use for longer than an unload may wait.
    TimedOut,
    /// The engine failed to release the file.
    Engine,
}

/// What the composition root gives the download manager.
pub struct TranslationDownloadsDeps {
    /// The application's shared transport: routes, clients and the operation barrier.
    pub transport: Arc<TransportHub>,
    /// The application runtime the download requests and file work run on.
    pub runtime: Runtime,
    /// Drops the translation engine that may hold the model file, before it is deleted.
    pub unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
}

/// Installs the download manager for the process.
pub fn install(cx: &mut App, deps: TranslationDownloadsDeps) {
    let downloads = cx.new(|_| TranslationDownloads::new(deps));
    cx.set_global(TranslationDownloadsGlobal { downloads });
}

/// The installed download manager, for the views that draw it.
pub(crate) fn entity(cx: &App) -> Option<Entity<TranslationDownloads>> {
    cx.try_global::<TranslationDownloadsGlobal>()
        .map(|installed| installed.downloads.clone())
}

/// The installed manager, in the application's global state.
struct TranslationDownloadsGlobal {
    downloads: Entity<TranslationDownloads>,
}

impl Global for TranslationDownloadsGlobal {}

/// What one row of the translation page is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum DownloadState {
    /// Nothing has happened to this model in this process.
    #[default]
    Idle,
    /// The file is being streamed to disk.
    Downloading { downloaded: u64 },
    /// The last download failed, and the row offers it again.
    DownloadFailed { failure: DownloadFailure },
    /// The model file is being unloaded and deleted.
    Deleting,
    /// The last delete failed, and the row says why.
    DeleteFailed { failure: DeleteFailure },
}

/// Why a translation model could not be downloaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownloadFailure {
    /// The server answered with a status that carries no model.
    Status { status: u16 },
    /// The request could not be sent or its body could not be read.
    Transport,
    /// The outbound route could not be resolved.
    Route,
    /// The bytes that arrived are not the size the model publishes.
    SizeMismatch { expected: u64, actual: u64 },
    /// The download was cancelled.
    Cancelled,
    /// A file could not be created, written, replaced or removed.
    Io,
    /// The compiled-in URL is not a URL.
    InvalidUrl,
    /// The destination has no file name, so no partial file can sit beside it.
    InvalidDestination,
    /// A redirect could not be followed.
    Redirect,
    /// The runtime dropped the task before it could report.
    Task,
}

/// Why a translation model file could not be deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeleteFailure {
    /// The translation engine still held the file and could not be unloaded.
    Unload(UnloadFailure),
    /// The file itself could not be removed.
    Io { kind: io::ErrorKind },
    /// The cancelled download of the model did not stop in time.
    DownloadsDidNotStop,
    /// The delete worker did not finish in time.
    TimedOut,
}

/// The process-wide model downloads and what each of them last did.
pub(crate) struct TranslationDownloads {
    transport: Arc<TransportHub>,
    runtime: Runtime,
    unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
    /// The last outcome of each model in each directory, keyed by both.
    states: AHashMap<(TranslationModel, PathBuf), DownloadState>,
    /// The tasks of each model in each directory. They are held because dropping a GPUI task
    /// cancels it: this is what keeps a download running after the settings page closes.
    tasks: AHashMap<(TranslationModel, PathBuf), Vec<Task<()>>>,
    /// The cancellation signal of each download in flight, so deleting its model can stop it.
    cancellations: AHashMap<(TranslationModel, PathBuf), CancellationToken>,
    /// The directory of the most recent completed download, even when the settings session that
    /// started it was never confirmed.
    last_download_directory: Option<PathBuf>,
    /// Models a test treats as downloaded without creating a multi-gigabyte file.
    #[cfg(test)]
    forced_downloaded: ahash::AHashSet<(TranslationModel, PathBuf)>,
}

impl TranslationDownloads {
    pub(crate) fn new(deps: TranslationDownloadsDeps) -> Self {
        let capacity = TranslationModel::ALL.len();

        Self {
            transport: deps.transport,
            runtime: deps.runtime,
            unload: deps.unload,
            states: AHashMap::with_capacity(capacity),
            tasks: AHashMap::with_capacity(capacity),
            cancellations: AHashMap::with_capacity(capacity),
            last_download_directory: None,
            #[cfg(test)]
            forced_downloaded: ahash::AHashSet::with_capacity(capacity),
        }
    }

    /// The directory of the most recent completed download, with or without a confirmed
    /// settings session.
    pub(crate) fn last_download_directory(&self) -> Option<&Path> {
        self.last_download_directory.as_deref()
    }

    /// What one row of `model` in `directory` last did.
    pub(crate) fn row_state(&self, model: TranslationModel, directory: &Path) -> DownloadState {
        self.states
            .get(&(model, directory.to_path_buf()))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the model's file is present in `directory` at its published size.
    pub(crate) fn is_downloaded(&self, model: TranslationModel, directory: &Path) -> bool {
        #[cfg(test)]
        if self
            .forced_downloaded
            .contains(&(model, directory.to_path_buf()))
        {
            return true;
        }

        file_size_matches(&directory.join(model.file_name()), model.size_bytes())
    }

    /// Every model that is downloaded in `directory`, in page order.
    pub(crate) fn downloaded_models(&self, directory: &Path) -> Vec<TranslationModel> {
        TranslationModel::ALL
            .into_iter()
            .filter(|model| self.is_downloaded(*model, directory))
            .collect()
    }

    /// Starts downloading `model` into `directory` through the configured route.
    pub(crate) fn download(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        network_config: Config,
        cx: &mut Context<Self>,
    ) {
        if self.is_downloaded(model, &directory)
            || matches!(
                self.row_state(model, &directory),
                DownloadState::Downloading { .. } | DownloadState::Deleting
            )
        {
            return;
        }

        let key = (model, directory.clone());
        self.states
            .insert(key.clone(), DownloadState::Downloading { downloaded: 0 });
        cx.notify();

        let transport = Arc::clone(&self.transport);
        let runtime = self.runtime.clone();
        let task_directory = directory.clone();
        let cancellation = self.transport.cancellation().child_token();
        self.cancellations.insert(key.clone(), cancellation.clone());
        let (progress_sender, mut progress_receiver) = mpsc::unbounded_channel();

        let downloaded = self.runtime.run(async move {
            download_model(
                &transport,
                &network_config,
                &runtime,
                model,
                &task_directory,
                &cancellation,
                move |bytes, total| {
                    // The reader runs on the runtime; this only hands the numbers to the
                    // interface task below.
                    let _ = progress_sender.send((bytes, total));
                },
            )
            .await
        });

        let progress_directory = directory.clone();
        let progress = cx.spawn(async move |downloads, cx| {
            while let Some((downloaded, total)) = progress_receiver.recv().await {
                let _ = downloads.update(cx, |downloads, cx| {
                    downloads.record_progress(model, &progress_directory, downloaded, total, cx)
                });
            }
        });

        let outcome_directory = directory;
        let outcome = cx.spawn(async move |downloads, cx| {
            let result = match downloaded.await {
                Ok(result) => result,
                Err(error) => Err(ModelDownloadError::Task(error)),
            };
            let _ = downloads.update(cx, |downloads, cx| {
                downloads.finish_download(model, outcome_directory, result, cx)
            });
        });

        self.tasks.insert(key, vec![progress, outcome]);
    }

    /// Records the bytes a download has accepted.
    fn record_progress(
        &mut self,
        model: TranslationModel,
        directory: &Path,
        downloaded: u64,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        let key = (model, directory.to_path_buf());
        let Some(DownloadState::Downloading {
            downloaded: accepted,
        }) = self.states.get_mut(&key)
        else {
            return;
        };
        // The reader reports only what it accepted; a number below what is already known is a
        // stale report, and one above the published size is a report the download is failing
        // on rather than progress to draw.
        if downloaded >= *accepted && downloaded <= total {
            *accepted = downloaded;
            cx.notify();
        }
    }

    /// Applies what a download produced.
    fn finish_download(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        result: Result<PathBuf, ModelDownloadError>,
        cx: &mut Context<Self>,
    ) {
        let key = (model, directory.clone());
        self.cancellations.remove(&key);
        // A delete in progress cancelled this download; its own outcome is what finishes now.
        if matches!(self.states.get(&key), Some(DownloadState::Deleting)) {
            return;
        }

        match result {
            Ok(_) => self.complete_download(model, directory, cx),
            Err(error) => {
                self.states.insert(
                    key,
                    DownloadState::DownloadFailed {
                        failure: error.into_failure(),
                    },
                );
                cx.notify();
            }
        }
    }

    /// Records a download that finished, selecting it when nothing usable is selected.
    fn complete_download(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.states.remove(&(model, directory.clone()));
        self.last_download_directory = Some(directory.clone());

        let auto_select = cx.try_global::<ConfigManager>().is_some_and(|manager| {
            let config = manager.read();
            config.translation.model_directory.as_ref() == Some(&directory)
                && !config
                    .translation
                    .selected_model
                    .is_some_and(|selected| self.is_downloaded(selected, &directory))
        });
        if auto_select {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.translation.selected_model = Some(model), cx);
            });
        }

        cx.emit(TranslationDownloadsEvent::Completed { model, directory });
        cx.notify();
    }

    /// Deletes `model` from `directory`, cancelling a download of it first and unloading it
    /// before the file goes.
    pub(crate) fn delete(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let key = (model, directory.clone());
        if matches!(self.row_state(model, &directory), DownloadState::Deleting) {
            return;
        }

        let clears_selection = cx.try_global::<ConfigManager>().is_some_and(|manager| {
            let config = manager.read();
            config.translation.model_directory.as_ref() == Some(&directory)
                && config.translation.selected_model == Some(model)
        });
        if clears_selection {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.translation.selected_model = None, cx);
            });
        }

        self.states.insert(key.clone(), DownloadState::Deleting);
        if let Some(cancellation) = self.cancellations.remove(&key) {
            cancellation.cancel();
        }
        let pending = self.tasks.remove(&key).unwrap_or_default();

        let runtime = self.runtime.clone();
        let unload = Arc::clone(&self.unload);
        let path = directory.join(model.file_name());
        let outcome_directory = directory;
        let outcome = cx.spawn(async move |downloads, cx| {
            // Cancelled downloads report through their own outcome task; waiting for it here
            // is what keeps the file from being removed under a reader that is still writing.
            // The wait is bounded so a reader that never stops cannot pin the row in Deleting.
            let clock = cx.background_executor().clone();
            let downloads_stopped = wait_for_downloads(pending, &clock).await;
            if !downloads_stopped {
                let _ = downloads.update(cx, |downloads, cx| {
                    log::warn!("the download of translation model {model:?} did not stop in time");
                    downloads.finish_delete(
                        model,
                        outcome_directory,
                        Err(DeleteFailure::DownloadsDidNotStop),
                        cx,
                    )
                });
                return;
            }

            let completion = Arc::new(Mutex::new(None));
            let worker_completion = Arc::clone(&completion);
            let worker_unload = Arc::clone(&unload);
            let worker_path = path;
            drop(runtime.blocking(move || {
                let result = (|| {
                    worker_unload(&worker_path).map_err(DeleteFailure::Unload)?;
                    if let Ok(part) = part_path(&worker_path) {
                        // Best effort: a missing or locked partial file must not stop the
                        // model itself from being deleted.
                        let _ = std::fs::remove_file(part);
                    }
                    match std::fs::remove_file(&worker_path) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                        Err(source) => Err(DeleteFailure::Io {
                            kind: source.kind(),
                        }),
                    }
                })();
                *worker_completion
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(result);
            }));

            // Poll on the interface executor instead of awaiting the blocking JoinHandle:
            // the completion is delivered from a worker thread, which is exactly what a GPUI
            // task must not be woken by in a deterministic test. The wait is bounded so a
            // worker that never reports leaves the row failed rather than stuck in Deleting.
            let deadline = clock.now() + DELETE_TIMEOUT;
            let result = loop {
                if let Some(result) = completion
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                {
                    break result;
                }
                if clock.now() >= deadline {
                    break Err(DeleteFailure::TimedOut);
                }
                clock.timer(DELETE_POLL_INTERVAL).await;
            };
            let _ = downloads.update(cx, |downloads, cx| {
                if let Err(error) = &result {
                    log::warn!("failed to delete translation model {model:?}: {error:?}");
                }
                downloads.finish_delete(model, outcome_directory, result, cx)
            });
        });

        self.tasks.entry(key).or_default().push(outcome);
        cx.notify();
    }

    /// Applies what a delete produced.
    fn finish_delete(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        result: Result<(), DeleteFailure>,
        cx: &mut Context<Self>,
    ) {
        let outcome = match result {
            Ok(()) => Ok(()),
            // The file is already gone, which is what the delete asked for.
            Err(DeleteFailure::Io {
                kind: io::ErrorKind::NotFound,
            }) => Ok(()),
            Err(failure) => Err(failure),
        };

        match outcome {
            Ok(()) => {
                self.states.remove(&(model, directory.clone()));
                #[cfg(test)]
                self.forced_downloaded.remove(&(model, directory.clone()));
                cx.emit(TranslationDownloadsEvent::Deleted { model, directory });
            }
            Err(failure) => {
                self.states
                    .insert((model, directory), DownloadState::DeleteFailed { failure });
            }
        }
        cx.notify();
    }

    /// Treats `model` as downloaded without creating a multi-gigabyte file.
    #[cfg(test)]
    pub(crate) fn force_downloaded_for_test(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.forced_downloaded.insert((model, directory));
        cx.notify();
    }

    /// Records the directory of a download without starting one.
    #[cfg(test)]
    pub(crate) fn set_last_download_directory_for_test(&mut self, directory: PathBuf) {
        self.last_download_directory = Some(directory);
    }

    /// Puts one row into a state a test draws without a network.
    #[cfg(test)]
    pub(crate) fn set_state_for_test(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        state: DownloadState,
        cx: &mut Context<Self>,
    ) {
        self.states.insert((model, directory), state);
        cx.notify();
    }

    /// Records a finished download without a network, through the production completion path.
    #[cfg(test)]
    pub(crate) fn complete_download_for_test(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.forced_downloaded.insert((model, directory.clone()));
        self.complete_download(model, directory, cx);
    }
}

/// What the translation page listens for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranslationDownloadsEvent {
    /// A download finished and its file is in place.
    Completed {
        model: TranslationModel,
        directory: PathBuf,
    },
    /// A model was deleted.
    Deleted {
        model: TranslationModel,
        directory: PathBuf,
    },
}

impl EventEmitter<TranslationDownloadsEvent> for TranslationDownloads {}

/// Whether `path` is a file of exactly `expected` bytes.
fn file_size_matches(path: &Path, expected: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected)
}

/// Waits for the cancelled downloads of one model to stop, or reports that they did not.
async fn wait_for_downloads(pending: Vec<Task<()>>, clock: &BackgroundExecutor) -> bool {
    let deadline = clock.now() + PENDING_DOWNLOAD_TIMEOUT;
    while !pending.iter().all(Task::is_ready) {
        if clock.now() >= deadline {
            return false;
        }
        clock.timer(DELETE_POLL_INTERVAL).await;
    }
    true
}

/// Why a translation model file could not be deleted.
#[derive(Debug, Error)]
enum ModelDownloadError {
    /// The directory the model is stored in could not be created.
    #[error("failed to create translation model directory {path:?}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The partial file could not be created.
    #[error("failed to create translation model file {path:?}: {source}")]
    CreateFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The downloaded bytes could not be written to disk.
    #[error("failed to write translation model file {path:?}: {source}")]
    WriteFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// An existing model file could not be removed before the new one replaced it.
    #[error("failed to remove existing translation model file {path:?}: {source}")]
    RemoveFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The completed partial file could not be renamed to the final path.
    #[error("failed to replace translation model file {path:?}: {source}")]
    ReplaceFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The destination has no file name, so no partial file can sit beside it.
    #[error("the translation model destination {path:?} has no file name")]
    InvalidDestination { path: PathBuf },
    /// The compiled-in HuggingFace URL is not a URL.
    #[error("the translation model URL is invalid")]
    InvalidUrl(#[source] ParseError),
    /// The request could not be sent at all.
    #[error("the translation model request could not be sent")]
    Request(#[source] reqwest::Error),
    /// The response body could not be read.
    #[error("failed while reading the translation model response")]
    Read(#[source] reqwest::Error),
    /// The server answered with a status that carries no model.
    #[error("the translation model download was answered with HTTP {status}")]
    Status { status: StatusCode },
    /// A redirect did not say where it goes.
    #[error("the translation model download redirect named no location")]
    RedirectMissingLocation,
    /// The redirect chain is longer than the download class allows.
    #[error("the translation model download exceeded its redirect limit")]
    RedirectLimit,
    /// A redirect location is not an HTTP or HTTPS URL.
    #[error("the translation model download redirect {location:?} is not a usable URL")]
    InvalidRedirect { location: SmolStr },
    /// Following a redirect would leave encrypted transport.
    #[error("the translation model download would leave https")]
    InsecureRedirect,
    /// The download was cancelled.
    #[error("the translation model download was cancelled")]
    Cancelled,
    /// The bytes that arrived are not the size the model publishes.
    #[error("the translation model download was {actual} bytes but {expected} were expected")]
    SizeMismatch { expected: u64, actual: u64 },
    /// The outbound route for the download could not be resolved.
    #[error("the translation model route could not be resolved")]
    Route(#[source] ConfigError),
    /// The transport could not start the operation or build its client.
    #[error("the translation model transport failed")]
    Transport(#[source] TransportError),
    /// The runtime dropped the task before it could report.
    #[error("the translation model task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl ModelDownloadError {
    /// The typed failure the page draws, with no message of its own.
    fn into_failure(self) -> DownloadFailure {
        match self {
            Self::CreateDirectory { .. }
            | Self::CreateFile { .. }
            | Self::WriteFile { .. }
            | Self::RemoveFile { .. }
            | Self::ReplaceFile { .. } => DownloadFailure::Io,
            Self::InvalidDestination { .. } => DownloadFailure::InvalidDestination,
            Self::InvalidUrl(_) => DownloadFailure::InvalidUrl,
            Self::Request(_) | Self::Read(_) => DownloadFailure::Transport,
            Self::Status { status } => DownloadFailure::Status {
                status: status.as_u16(),
            },
            Self::RedirectMissingLocation | Self::RedirectLimit | Self::InvalidRedirect { .. } => {
                DownloadFailure::Redirect
            }
            Self::InsecureRedirect => DownloadFailure::Redirect,
            Self::Cancelled => DownloadFailure::Cancelled,
            Self::SizeMismatch { expected, actual } => {
                DownloadFailure::SizeMismatch { expected, actual }
            }
            Self::Route(_) => DownloadFailure::Route,
            Self::Transport(_) => DownloadFailure::Transport,
            Self::Task(_) => DownloadFailure::Task,
        }
    }
}

/// Downloads `model` into `directory` and returns the final file path.
async fn download_model(
    transport: &TransportHub,
    network_config: &Config,
    runtime: &Runtime,
    model: TranslationModel,
    directory: &Path,
    cancellation: &CancellationToken,
    on_progress: impl Fn(u64, u64) + Send + 'static,
) -> Result<PathBuf, ModelDownloadError> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|source| ModelDownloadError::CreateDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
    let url = Url::parse(model.download_url()).map_err(ModelDownloadError::InvalidUrl)?;
    let destination = directory.join(model.file_name());

    download_file_from_url(
        transport,
        network_config,
        runtime,
        url,
        &destination,
        model.size_bytes(),
        cancellation,
        on_progress,
    )
    .await?;

    Ok(destination)
}

/// The implementation behind [`download_model`], with its source URL and expected size
/// explicit so a test can point it at a local server and a small fixture.
async fn download_file_from_url(
    transport: &TransportHub,
    network_config: &Config,
    runtime: &Runtime,
    url: Url,
    destination: &Path,
    expected_size: u64,
    cancellation: &CancellationToken,
    on_progress: impl Fn(u64, u64) + Send + 'static,
) -> Result<(), ModelDownloadError> {
    if cancellation.is_cancelled() {
        return Err(ModelDownloadError::Cancelled);
    }

    let _operation = transport
        .begin_operation()
        .map_err(ModelDownloadError::Transport)?;
    let part_path = part_path(destination)?;
    let outcome = download_to_part(
        transport,
        network_config,
        runtime,
        url,
        &part_path,
        expected_size,
        cancellation,
        on_progress,
    )
    .await;
    let outcome = match outcome {
        Ok(()) => replace_file(&part_path, destination).await,
        Err(error) => Err(error),
    };

    if outcome.is_err() {
        // Best effort: the error the caller sees is the one that stopped the download, not a
        // cleanup failure that would hide it.
        let _ = tokio::fs::remove_file(&part_path).await;
    }

    outcome
}

/// Streams one response into `part_path`, returning only after the byte count matches.
async fn download_to_part(
    transport: &TransportHub,
    network_config: &Config,
    runtime: &Runtime,
    url: Url,
    part_path: &Path,
    expected_size: u64,
    cancellation: &CancellationToken,
    on_progress: impl Fn(u64, u64),
) -> Result<(), ModelDownloadError> {
    on_progress(0, expected_size);

    let mut file =
        File::create(part_path)
            .await
            .map_err(|source| ModelDownloadError::CreateFile {
                path: part_path.to_path_buf(),
                source,
            })?;
    let mut response = open_response(transport, network_config, runtime, url, cancellation).await?;
    let mut written = 0_u64;
    let mut last_reported_bytes = 0_u64;
    let mut last_reported_at = Instant::now();

    loop {
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ModelDownloadError::Cancelled),
            chunk = response.chunk() => chunk.map_err(ModelDownloadError::Read)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        written = written.saturating_add(chunk.len() as u64);
        if written > expected_size {
            return Err(ModelDownloadError::SizeMismatch {
                expected: expected_size,
                actual: written,
            });
        }
        file.write_all(&chunk)
            .await
            .map_err(|source| ModelDownloadError::WriteFile {
                path: part_path.to_path_buf(),
                source,
            })?;
        if written.saturating_sub(last_reported_bytes) >= PROGRESS_REPORT_BYTES
            || last_reported_at.elapsed() >= PROGRESS_REPORT_INTERVAL
        {
            on_progress(written, expected_size);
            last_reported_bytes = written;
            last_reported_at = Instant::now();
        }
    }

    file.flush()
        .await
        .map_err(|source| ModelDownloadError::WriteFile {
            path: part_path.to_path_buf(),
            source,
        })?;
    if written != expected_size {
        return Err(ModelDownloadError::SizeMismatch {
            expected: expected_size,
            actual: written,
        });
    }
    on_progress(expected_size, expected_size);

    Ok(())
}

/// Sends the request and follows its redirects, returning only a successful final response.
///
/// Every hop is resolved and sent through the model download class, so a route that is
/// destination-dependent applies to the final destination rather than being inherited from the
/// first URL.
async fn open_response(
    transport: &TransportHub,
    network_config: &Config,
    runtime: &Runtime,
    mut url: Url,
    cancellation: &CancellationToken,
) -> Result<Response, ModelDownloadError> {
    let target = RouteTarget::Service(ServiceTarget::TranslationModelDownload);
    let limit = HttpClientClass::ModelDownload.redirect_limit();
    let mut followed = 0_usize;

    loop {
        let route = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ModelDownloadError::Cancelled),
            route = resolve_async_cancellable(
                network_config,
                &target,
                &url,
                cancellation.child_token(),
                runtime,
            ) => route.map_err(ModelDownloadError::Route)?,
        };
        let client = transport
            .http()
            .client(&route, HttpClientClass::ModelDownload)
            .map_err(ModelDownloadError::Transport)?;
        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ModelDownloadError::Cancelled),
            response = client
                .get(url.clone())
                .header(ACCEPT_ENCODING, "identity")
                .send() => response.map_err(ModelDownloadError::Request)?,
        };

        let status = response.status();
        if !status.is_redirection() || status == StatusCode::NOT_MODIFIED {
            if !status.is_success() {
                return Err(ModelDownloadError::Status { status });
            }

            return Ok(response);
        }

        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(ModelDownloadError::RedirectMissingLocation)?;
        if followed >= limit {
            return Err(ModelDownloadError::RedirectLimit);
        }
        let next = url
            .join(location)
            .map_err(|_| ModelDownloadError::InvalidRedirect {
                location: SmolStr::new(location),
            })?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(ModelDownloadError::InvalidRedirect {
                location: SmolStr::new(location),
            });
        }
        if url.scheme() == "https" && next.scheme() != "https" {
            return Err(ModelDownloadError::InsecureRedirect);
        }

        followed += 1;
        url = next;
    }
}

/// Removes an existing destination, if any, and renames the completed part into its place.
async fn replace_file(part_path: &Path, destination: &Path) -> Result<(), ModelDownloadError> {
    match tokio::fs::remove_file(destination).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ModelDownloadError::RemoveFile {
                path: destination.to_path_buf(),
                source,
            });
        }
    }
    tokio::fs::rename(part_path, destination)
        .await
        .map_err(|source| ModelDownloadError::ReplaceFile {
            path: destination.to_path_buf(),
            source,
        })
}

/// The partial file a download streams into before it is renamed into place.
fn part_path(destination: &Path) -> Result<PathBuf, ModelDownloadError> {
    let mut file_name = destination
        .file_name()
        .ok_or_else(|| ModelDownloadError::InvalidDestination {
            path: destination.to_path_buf(),
        })?
        .to_os_string();
    file_name.push(".part");

    Ok(destination.with_file_name(file_name))
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        io,
        net::{Ipv4Addr, SocketAddr},
        path::Path,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
        time::Duration,
    };

    use app_config::translation::TranslationModel;
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use hyper::{
        Request, Response, StatusCode,
        body::{Body, Bytes, Frame, Incoming, SizeHint},
        header::LOCATION,
        server::conn::http1,
        service::service_fn,
    };
    use hyper_util::rt::TokioIo;
    use network::transport::hub::TransportPolicy;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;
    use widget_tokio_runtime::runtime::Runtime;

    use super::{
        DeleteFailure, DownloadFailure, DownloadState, ModelDownloadError,
        PENDING_DOWNLOAD_TIMEOUT, TranslationDownloads, TranslationDownloadsDeps, UnloadFailure,
        download_file_from_url, file_size_matches,
    };

    /// A body the test server hands out as one frame.
    struct FixedBody(Option<Bytes>);

    impl Body for FixedBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(self.0.take().map(|bytes| Ok(Frame::data(bytes))))
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(self.0.as_ref().map_or(0, |bytes| bytes.len() as u64))
        }
    }

    /// Answers one request with `body` after redirecting from `/model` to `/payload`.
    async fn spawn_redirect_server(body: &'static [u8]) -> SocketAddr {
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
                        "/model" => Response::builder()
                            .status(StatusCode::FOUND)
                            .header(LOCATION, "/payload")
                            .body(FixedBody(None))
                            .expect("the redirect is well formed"),
                        _ => Response::builder()
                            .status(StatusCode::OK)
                            .body(FixedBody(Some(Bytes::from_static(body))))
                            .expect("the response is well formed"),
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

    fn deps() -> TranslationDownloadsDeps {
        deps_with_unload(Arc::new(|_: &Path| Ok(())))
    }

    fn deps_with_unload(
        unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
    ) -> TranslationDownloadsDeps {
        TranslationDownloadsDeps {
            transport: Arc::new(network::transport::hub::TransportHub::new(
                TransportPolicy {
                    allow_plaintext: true,
                    extra_root_certificates: Vec::new(),
                },
            )),
            runtime: Runtime::for_test(),
            unload,
        }
    }

    /// Waits for a delete that has no callback of its own to settle.
    fn wait_until_deleted(
        cx: &mut TestAppContext,
        downloads: &Entity<TranslationDownloads>,
        model: TranslationModel,
        directory: &Path,
        path: &Path,
    ) {
        for _ in 0..200 {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            if !path.exists()
                && cx.update(|cx| downloads.read(cx).row_state(model, directory))
                    == DownloadState::Idle
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the delete should finish: {path:?} still exists");
    }

    /// Deleting a file that is already gone is what the row asked for, not a failure.
    #[gpui_kit::test]
    fn deleting_a_missing_file_should_succeed(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let path = directory.path().join(model.file_name());

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until_deleted(cx, &downloads, model, directory.path(), &path);

        assert!(!path.exists());
        assert_eq!(
            cx.update(|cx| downloads.read(cx).row_state(model, directory.path())),
            DownloadState::Idle,
            "a delete that found nothing to remove must not be left failed"
        );
    }

    /// The engine is unloaded while the file still exists; only then is the file removed.
    #[gpui_kit::test]
    fn a_delete_should_unload_before_it_removes_the_file(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let path = directory.path().join(model.file_name());
        std::fs::write(&path, b"model").expect("the fixture file is written");
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_for_unload = Arc::clone(&order);
        let downloads = cx.update(|cx| {
            cx.new(|_| {
                TranslationDownloads::new(deps_with_unload(Arc::new(move |path: &Path| {
                    let state = if path.exists() {
                        "unload-before-remove"
                    } else {
                        "unload-after-remove"
                    };
                    order_for_unload
                        .lock()
                        .expect("the order log is not poisoned")
                        .push(state);
                    Ok(())
                })))
            })
        });

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until_deleted(cx, &downloads, model, directory.path(), &path);

        assert_eq!(
            *order.lock().expect("the order log is not poisoned"),
            ["unload-before-remove"],
            "the engine must release the file before the delete removes it"
        );
    }

    /// Deleting a model cancels a download of it before waiting for that download to stop.
    #[gpui_kit::test]
    fn deleting_a_model_should_cancel_its_download(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let key = (model, directory.path().to_path_buf());
        let cancellation = CancellationToken::new();
        let token = cancellation.clone();
        let path = directory.path().join(model.file_name());

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.cancellations.insert(key, cancellation);
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until_deleted(cx, &downloads, model, directory.path(), &path);

        assert!(
            token.is_cancelled(),
            "the download of a deleted model must be cancelled first"
        );
    }

    /// A finished delete that found the file already gone is still success.
    #[gpui_kit::test]
    fn finish_delete_should_accept_a_missing_file(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.finish_delete(
                    model,
                    directory.path().to_path_buf(),
                    Err(DeleteFailure::Io {
                        kind: io::ErrorKind::NotFound,
                    }),
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|cx| downloads.read(cx).row_state(model, directory.path())),
            DownloadState::Idle
        );
    }

    /// A failed unload leaves the row reporting the reason instead of hanging in Deleting.
    #[gpui_kit::test]
    fn finish_delete_should_report_an_unload_failure(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.finish_delete(
                    model,
                    directory.path().to_path_buf(),
                    Err(DeleteFailure::Unload(UnloadFailure::TimedOut)),
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|cx| downloads.read(cx).row_state(model, directory.path())),
            DownloadState::DeleteFailed {
                failure: DeleteFailure::Unload(UnloadFailure::TimedOut),
            },
            "an unload failure should become DeleteFailed"
        );
    }

    /// A failed download does not become the directory a later settings session opens on.
    #[gpui_kit::test]
    fn a_failed_download_should_not_record_its_directory(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.finish_download(
                    model,
                    directory.path().to_path_buf(),
                    Err(ModelDownloadError::Cancelled),
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|cx| downloads
                .read(cx)
                .last_download_directory()
                .map(Path::to_path_buf)),
            None,
            "a failed download must not become the directory the page opens on"
        );
        assert_eq!(
            cx.update(|cx| downloads.read(cx).row_state(model, directory.path())),
            DownloadState::DownloadFailed {
                failure: DownloadFailure::Cancelled,
            }
        );
    }

    /// A completed download does record its directory, even when the session that started it
    /// was never confirmed.
    #[gpui_kit::test]
    fn a_completed_download_should_record_its_directory(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.complete_download_for_test(model, directory.path().to_path_buf(), cx);
            });
        });

        assert_eq!(
            cx.update(|cx| downloads
                .read(cx)
                .last_download_directory()
                .map(Path::to_path_buf)),
            Some(directory.path().to_path_buf())
        );
    }

    /// The failure the page draws is typed, so it is translated rather than best-effort text.
    #[test]
    fn download_errors_should_become_typed_failures() {
        assert_eq!(
            ModelDownloadError::Status {
                status: StatusCode::NOT_FOUND,
            }
            .into_failure(),
            DownloadFailure::Status { status: 404 }
        );
        assert_eq!(
            ModelDownloadError::Cancelled.into_failure(),
            DownloadFailure::Cancelled
        );
        assert_eq!(
            ModelDownloadError::CreateFile {
                path: "model.gguf.part".into(),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            }
            .into_failure(),
            DownloadFailure::Io
        );
    }

    /// A delete waiting for a cancelled download that never stops must fail rather than stay
    /// in Deleting forever.
    #[gpui_kit::test]
    fn a_delete_whose_download_never_stops_should_be_reported_as_failed(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let key = (model, directory.path().to_path_buf());

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                let stuck = cx.spawn(async move |_, _| {
                    std::future::pending::<()>().await;
                });
                downloads.tasks.entry(key).or_default().push(stuck);
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });

        cx.executor()
            .advance_clock(PENDING_DOWNLOAD_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();

        assert_eq!(
            cx.update(|cx| downloads.read(cx).row_state(model, directory.path())),
            DownloadState::DeleteFailed {
                failure: DeleteFailure::DownloadsDidNotStop,
            },
            "a download that never stops must be reported instead of staying Deleting"
        );
    }

    /// A delete worker that never finishes is reported as failed instead of staying Deleting.
    #[gpui_kit::test]
    fn a_delete_that_never_finishes_should_be_reported_as_failed(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let path = directory.path().join(model.file_name());
        std::fs::write(&path, b"model").expect("the fixture file is written");
        let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let release_receiver = Mutex::new(release_receiver);
        let downloads = cx.update(|cx| {
            cx.new(|_| {
                TranslationDownloads::new(deps_with_unload(Arc::new(move |_: &Path| {
                    entered_sender
                        .send(())
                        .expect("the test should be waiting for the unload");
                    release_receiver
                        .lock()
                        .expect("the release channel is not poisoned")
                        .recv()
                        .expect("the test should release the unload");
                    Ok(())
                })))
            })
        });

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        // The worker only starts once the spawned delete task has been polled, so the
        // executor is turned while the test waits for the unload to be entered.
        let mut entered = false;
        for _ in 0..200 {
            cx.run_until_parked();
            if entered_receiver.try_recv().is_ok() {
                entered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(entered, "the delete should reach its unload");

        cx.executor()
            .advance_clock(super::DELETE_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();

        let state = cx.update(|cx| downloads.read(cx).row_state(model, directory.path()));
        assert!(
            matches!(state, DownloadState::DeleteFailed { .. }),
            "a delete that never finishes must not stay Deleting: {state:?}"
        );

        release_sender
            .send(())
            .expect("the delete worker should still be waiting");
    }

    /// A file of exactly the expected size counts; a short one does not.
    #[test]
    fn a_file_should_have_to_match_the_expected_size() {
        let directory = TempDir::new().expect("the test directory builds");
        let path = directory.path().join("model.gguf");
        std::fs::write(&path, b"1234").expect("the fixture file is written");

        assert!(file_size_matches(&path, 4));
        assert!(!file_size_matches(&path, 5));
        assert!(!file_size_matches(&directory.path().join("absent.gguf"), 4));
    }

    /// A model a test marks as downloaded is downloaded without a file; the order is page order.
    #[test]
    fn downloaded_models_should_filter_in_page_order() {
        let downloads = TranslationDownloads::new(deps());
        let directory = TempDir::new().expect("the test directory builds");

        assert!(downloads.downloaded_models(directory.path()).is_empty());
    }

    #[gpui_kit::test]
    fn a_forced_model_should_be_reported_downloaded_and_in_page_order(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let directory = TempDir::new().expect("the test directory builds");
        let path = directory.path().to_path_buf();

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.force_downloaded_for_test(TranslationModel::HyMt27BQ8, path.clone(), cx);
                downloads.force_downloaded_for_test(
                    TranslationModel::HyMt218BQ4Km,
                    path.clone(),
                    cx,
                );
            });
        });

        let models = cx.update(|cx| downloads.read(cx).downloaded_models(&path));
        assert_eq!(
            models,
            vec![TranslationModel::HyMt218BQ4Km, TranslationModel::HyMt27BQ8],
            "the list is the page order, not the order the files were marked in"
        );
    }

    #[gpui_kit::test]
    fn state_should_default_to_idle_and_carry_what_was_injected(cx: &mut TestAppContext) {
        let downloads = cx.update(|cx| cx.new(|_| TranslationDownloads::new(deps())));
        let path = TempDir::new()
            .expect("the test directory builds")
            .path()
            .to_path_buf();

        assert_eq!(
            cx.update(|cx| downloads
                .read(cx)
                .row_state(TranslationModel::HyMt218BQ8, &path)),
            DownloadState::Idle
        );

        cx.update(|cx| {
            downloads.update(cx, |downloads, cx| {
                downloads.set_state_for_test(
                    TranslationModel::HyMt218BQ8,
                    path.clone(),
                    DownloadState::Downloading { downloaded: 7 },
                    cx,
                );
            });
        });
        assert_eq!(
            cx.update(|cx| downloads
                .read(cx)
                .row_state(TranslationModel::HyMt218BQ8, &path)),
            DownloadState::Downloading { downloaded: 7 }
        );
    }

    /// A download through a real transport writes the exact bytes and renames the part away.
    #[gpui_kit::test]
    fn a_complete_download_should_replace_the_destination(_cx: &mut TestAppContext) {
        let runtime = Runtime::for_test();
        let address = runtime
            .handle()
            .block_on(spawn_redirect_server(b"model-bytes"));
        let directory = TempDir::new().expect("the test directory builds");
        let destination = directory.path().join("model.gguf");
        std::fs::write(&destination, b"old").expect("the old file is written");
        let transport = std::sync::Arc::new(network::transport::hub::TransportHub::new(
            TransportPolicy {
                allow_plaintext: true,
                extra_root_certificates: Vec::new(),
            },
        ));
        let url = url::Url::parse(&format!("http://{address}/model")).expect("the URL is valid");
        let cancellation = tokio_util::sync::CancellationToken::new();
        let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress_for_task = std::sync::Arc::clone(&progress);

        let outcome = runtime.handle().block_on(download_file_from_url(
            &transport,
            &app_config::network::Config::default(),
            &runtime,
            url,
            &destination,
            b"model-bytes".len() as u64,
            &cancellation,
            move |bytes, total| {
                progress_for_task
                    .lock()
                    .expect("the progress log is not poisoned")
                    .push((bytes, total));
            },
        ));

        assert!(outcome.is_ok(), "the download should finish: {outcome:?}");
        assert_eq!(
            std::fs::read(&destination).expect("the destination is readable"),
            b"model-bytes"
        );
        assert!(
            !directory.path().join("model.gguf.part").exists(),
            "a completed download leaves no partial file"
        );
        assert_eq!(
            *progress.lock().expect("the progress log is not poisoned"),
            vec![(0, 11), (11, 11)],
            "the start and the end are always reported"
        );
    }

    /// A body shorter than the published size fails and leaves no part file.
    #[gpui_kit::test]
    fn a_short_download_should_fail_and_clean_up(_cx: &mut TestAppContext) {
        let runtime = Runtime::for_test();
        let address = runtime.handle().block_on(spawn_redirect_server(b"short"));
        let directory = TempDir::new().expect("the test directory builds");
        let destination = directory.path().join("model.gguf");
        let transport = std::sync::Arc::new(network::transport::hub::TransportHub::new(
            TransportPolicy {
                allow_plaintext: true,
                extra_root_certificates: Vec::new(),
            },
        ));
        let url = url::Url::parse(&format!("http://{address}/model")).expect("the URL is valid");

        let outcome = runtime.handle().block_on(download_file_from_url(
            &transport,
            &app_config::network::Config::default(),
            &runtime,
            url,
            &destination,
            12,
            &tokio_util::sync::CancellationToken::new(),
            |_, _| {},
        ));

        assert!(
            matches!(
                outcome,
                Err(ModelDownloadError::SizeMismatch {
                    expected: 12,
                    actual: 5
                })
            ),
            "the short body must be rejected: {outcome:?}"
        );
        assert!(!destination.exists());
        assert!(!directory.path().join("model.gguf.part").exists());
    }
}

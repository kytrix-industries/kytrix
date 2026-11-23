//! The process-wide translation model downloads.
//!
//! A download belongs to the process rather than to a settings session: closing the settings page
//! must not cancel a model that is still arriving, and opening the page again must show the
//! progress the download has reached. The manager is therefore created by the composition root
//! once, handed to the [`SettingsHost`](crate::host::SettingsHost) and shared by every page that
//! draws it.
//!
//! The transfers themselves belong to the application's download manager, which this crate reaches
//! through the injected [`DownloadService`] seam: adding, pausing, resuming and deleting a task are
//! commands, and the task snapshots the manager publishes are a signal. What stays here is the
//! translation model's own meaning — which file of which directory a row is, the engine unload that
//! has to happen before a file goes, and the directory and selection a finished download records.
//!
//! # Frames, not events
//!
//! There is no entity system to subscribe to: [`TranslationDownloads::sync`] is the per-frame half.
//! It mirrors the task snapshots the download manager published onto the rows, applies what the
//! blocking removals produced, and answers whether anything moved — a steady frame writes nothing
//! and asks for no frame (I3). A removal that finishes between frames wakes the application through
//! the [`AppHandle`] it was started with, so the result is picked up on the frame that follows
//! instead of being polled for.

use std::{
    cell::{Cell, RefCell},
    io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use ahash::AHashMap;
use app_config::{
    network::{RouteTarget, ServiceTarget},
    translation::{DownloadSite, TranslationModel},
};
use download_provider::{
    error::DownloadFailure,
    request::DownloadRequest,
    task::{DownloadId, DownloadStatus, DownloadTask},
};
use network::transport::http::HttpClientClass;
use ui_core::{
    app::{App, AppHandle},
    node::signal::Signal,
    window::WindowId,
};

use crate::session::{SettingsChange, SettingsStore};

/// How often a removal checks whether the engine unload on its worker thread has finished.
const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long a removal may wait for the engine unload before it is reported as failed.
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

/// The application's download manager, as the settings page can see it.
///
/// The commands are fire-and-forget: the manager publishes what became of a task through
/// [`Self::tasks`], which is the state the rows are drawn from. A command the manager refuses
/// therefore needs no answer — the snapshot simply does not change.
#[derive(Clone)]
pub struct DownloadService {
    /// Every task the manager last published, in the order it holds them.
    pub tasks: Signal<Vec<DownloadTask>>,
    /// Adds one download, under the identity the caller pre-allocated.
    pub add: Rc<dyn Fn(DownloadRequest, DownloadId)>,
    /// Stops a running transfer at its partial file.
    pub pause: Rc<dyn Fn(DownloadId)>,
    /// Continues a paused transfer from its partial file.
    pub resume: Rc<dyn Fn(DownloadId)>,
    /// Removes a task and its files.
    pub delete: Rc<dyn Fn(DownloadId)>,
}

/// What the composition root gives the download manager.
pub struct TranslationDownloadsDeps {
    /// The settings session a finished download records its directory and selection through.
    pub store: SettingsStore,
    /// The application's download manager.
    pub downloads: DownloadService,
    /// The application runtime the engine unload and the file removal run on.
    pub runtime: tokio::runtime::Handle,
    /// Drops the translation engine that may hold the model file, before it is deleted.
    pub unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
}

/// What one row of the translation page is doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum DownloadState {
    /// Nothing has happened to this model in this process.
    #[default]
    Idle,
    /// The file is being streamed to disk.
    Downloading {
        downloaded: u64,
        /// The rate the transfer wrote at over the interval this state was published for, in
        /// megabytes per second, as the download manager reports it.
        mb_per_second: f32,
    },
    /// The transfer is being stopped and will settle as [`Self::Paused`].
    Pausing { downloaded: u64 },
    /// The transfer is stopped and can be resumed.
    Paused { downloaded: u64 },
    /// The last download failed, and the row offers it again.
    DownloadFailed { failure: DownloadFailure },
    /// The model file is being unloaded and deleted.
    Deleting,
    /// What the download wrote is being removed after it was stopped for good.
    Terminating,
    /// The last delete failed, and the row says why.
    DeleteFailed { failure: DeleteFailure },
    /// The last stop failed, and the row says why.
    TerminateFailed { failure: DeleteFailure },
}

impl DownloadState {
    /// Which removal the row is in the middle of, when it is in one.
    fn removal(&self) -> Option<Removal> {
        match self {
            Self::Deleting => Some(Removal::Delete),
            Self::Terminating => Some(Removal::Terminate),
            _ => None,
        }
    }

    /// Whether the row's files are being removed, whichever action asked for it.
    pub(crate) fn is_removing(&self) -> bool {
        self.removal().is_some()
    }

    /// Whether the row reports a removal that failed, which stays until the user asks again.
    fn is_failed_removal(&self) -> bool {
        matches!(
            self,
            Self::DeleteFailed { .. } | Self::TerminateFailed { .. }
        )
    }
}

/// What the removal of a model's files was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    /// Delete a model whose file is in place.
    Delete,
    /// Stop a download for good and discard what it wrote.
    Terminate,
}

impl Removal {
    /// The state a row is drawn with while this removal runs.
    fn running(self) -> DownloadState {
        match self {
            Self::Delete => DownloadState::Deleting,
            Self::Terminate => DownloadState::Terminating,
        }
    }

    /// The state a row is drawn with when this removal failed.
    fn failed(self, failure: DeleteFailure) -> DownloadState {
        match self {
            Self::Delete => DownloadState::DeleteFailed { failure },
            Self::Terminate => DownloadState::TerminateFailed { failure },
        }
    }
}

/// Why a translation model file could not be deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeleteFailure {
    /// The translation engine still held the file and could not be unloaded.
    Unload(UnloadFailure),
    /// The file itself could not be removed.
    Io { kind: io::ErrorKind },
    /// The delete did not finish in time.
    TimedOut,
    /// The download manager failed the delete for a reason of its own.
    Provider(DownloadFailure),
}

/// The model, directory and manager task one row stands for.
type RowKey = (TranslationModel, PathBuf);

/// What one row's manager task is, whether a snapshot of it was seen, and the status it last
/// published.
#[derive(Clone, Debug)]
struct TaskEntry {
    id: DownloadId,
    /// Whether a task snapshot was seen for this id. A task the manager never published — one the
    /// application's download manager refused — is not one whose disappearance means a removal.
    seen: bool,
    /// The status the task published last. A download that had already failed when its row was
    /// deleted is not news; a failure after the delete is.
    last: Option<DownloadStatus>,
}

/// What a blocking removal worker was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingKind {
    /// The engine was unloaded; the task delete is issued once this is applied.
    UnloadTask(DownloadId),
    /// The files were unloaded and removed without a task.
    Files,
}

/// What one removal worker produced.
struct PendingOutcome {
    key: RowKey,
    removal: Removal,
    kind: PendingKind,
    result: Result<(), DeleteFailure>,
}

/// The process-wide model downloads and what each of them last did.
pub struct TranslationDownloads {
    store: SettingsStore,
    downloads: DownloadService,
    runtime: tokio::runtime::Handle,
    unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
    /// Requests the frame that applies a worker's result.
    wake: AppHandle,
    /// How long a removal waits for the engine unload; shortened by the tests.
    delete_timeout: Cell<Duration>,
    /// The last outcome of each model in each directory, keyed by both.
    states: RefCell<AHashMap<RowKey, DownloadState>>,
    /// The manager task of each model in each directory, which is what a pause, a resume or a
    /// delete of the row acts on.
    tasks: RefCell<AHashMap<RowKey, TaskEntry>>,
    /// The site each download was started from, which is what tells a retry whether the site the
    /// user chose continues that task or asks for a different transfer.
    sites: RefCell<AHashMap<RowKey, DownloadSite>>,
    /// The downloads a retry asked to start once the files of another site are gone.
    pending_restarts: RefCell<AHashMap<RowKey, DownloadSite>>,
    /// The removals that are in flight, which is what a stale task snapshot must not overwrite.
    removals: RefCell<AHashMap<RowKey, Removal>>,
    /// What the blocking removal workers produced and the frame has not applied yet.
    completions: Arc<Mutex<Vec<PendingOutcome>>>,
    /// Models a test treats as downloaded without creating a multi-gigabyte file.
    #[cfg(test)]
    forced_downloaded: RefCell<ahash::AHashSet<RowKey>>,
}

impl TranslationDownloads {
    /// The manager over the settings session and the application's download manager.
    pub fn new(app: &mut App, deps: TranslationDownloadsDeps) -> Rc<Self> {
        let capacity = TranslationModel::ALL.len();
        Rc::new(Self {
            store: deps.store,
            downloads: deps.downloads,
            runtime: deps.runtime,
            unload: deps.unload,
            wake: app.handle(),
            delete_timeout: Cell::new(DELETE_TIMEOUT),
            states: RefCell::new(AHashMap::with_capacity(capacity)),
            tasks: RefCell::new(AHashMap::with_capacity(capacity)),
            sites: RefCell::new(AHashMap::with_capacity(capacity)),
            pending_restarts: RefCell::new(AHashMap::with_capacity(capacity)),
            removals: RefCell::new(AHashMap::with_capacity(capacity)),
            completions: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            forced_downloaded: RefCell::new(ahash::AHashSet::with_capacity(capacity)),
        })
    }

    /// Mirrors the manager's task snapshots and the finished removals onto the rows.
    ///
    /// Returns whether anything moved. A row set that did not change asks for no frame (I3).
    pub(crate) fn sync(&self, app: &mut App) -> bool {
        let mut changed = false;
        for outcome in self.take_outcomes() {
            changed |= self.apply_outcome(app, outcome);
        }

        let published = app.read_with(&self.downloads.tasks, Clone::clone);
        let snapshots = published
            .iter()
            .map(|task| (task.id, task))
            .collect::<AHashMap<_, _>>();
        let known = self
            .tasks
            .borrow()
            .iter()
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect::<Vec<_>>();

        for (key, entry) in known {
            match snapshots.get(&entry.id) {
                Some(task) => {
                    self.mark_seen(&key);
                    let previous = self.observe_status(&key, &task.status);
                    changed |= self.apply_status(app, &key, previous.as_ref(), &task.status);
                }
                None if entry.seen => changed |= self.task_vanished(app, &key),
                None => {}
            }
        }

        changed
    }

    /// What one row of `model` in `directory` last did.
    pub(crate) fn row_state(&self, model: TranslationModel, directory: &Path) -> DownloadState {
        self.states
            .borrow()
            .get(&(model, directory.to_path_buf()))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the model's file is present in `directory` at its published size.
    pub(crate) fn is_downloaded(&self, model: TranslationModel, directory: &Path) -> bool {
        #[cfg(test)]
        if self
            .forced_downloaded
            .borrow()
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

    /// Whether `model` is one `directory` can load: its file is there at the published size and
    /// it is not being removed.
    pub(crate) fn is_available(&self, model: TranslationModel, directory: &Path) -> bool {
        !self.row_state(model, directory).is_removing() && self.is_downloaded(model, directory)
    }

    /// The models `directory` can load, in page order.
    pub(crate) fn available_models(&self, directory: &Path) -> Vec<TranslationModel> {
        self.downloaded_models(directory)
            .into_iter()
            .filter(|model| self.is_available(*model, directory))
            .collect()
    }

    /// The site the download of `model` in `directory` was started from, for the tests that pin
    /// what a retry asks the manager to do.
    #[cfg(test)]
    pub(crate) fn site(&self, model: TranslationModel, directory: &Path) -> Option<DownloadSite> {
        self.sites
            .borrow()
            .get(&(model, directory.to_path_buf()))
            .copied()
    }

    /// Starts downloading `model` into `directory` from `site`.
    ///
    /// This is what both a first download and the restart a retry asks for go through. A new
    /// transfer starts from the chosen site; a paused or failed task is continued only when the
    /// site it was started from is unknown or the same, because continuing it would report the
    /// bytes of one site under another.
    pub(crate) fn download(&self, model: TranslationModel, directory: PathBuf, site: DownloadSite) {
        let key = (model, directory.clone());
        // A task that exists from another site is left alone: resuming it, or recording this site
        // over it, would report the bytes of one site under another.
        let recorded_site = self.sites.borrow().get(&key).copied();
        let task = self.tasks.borrow().get(&key).cloned();
        if task.is_some() && recorded_site.is_some_and(|recorded| recorded != site) {
            return;
        }
        // A delete that is still in flight owns the files: a download started now would truncate
        // the partial file that delete is removing.
        let restart_pending = self.pending_restarts.borrow().contains_key(&key);
        if restart_pending
            || self.is_downloaded(model, &directory)
            || self.row_state(model, &directory).is_removing()
        {
            return;
        }

        if let Some(entry) = task {
            // The row's own state decides: a paused or failed task continues from its partial
            // file, while anything else is already doing what the row asked for.
            match self.row_state(model, &directory) {
                DownloadState::Paused { .. } | DownloadState::DownloadFailed { .. } => {
                    (self.downloads.resume)(entry.id);
                }
                _ => {}
            }
            return;
        }

        let Ok(url) = url::Url::parse(model.download_url(site)) else {
            self.set_state(
                key,
                DownloadState::DownloadFailed {
                    failure: DownloadFailure::InvalidUrl,
                },
            );
            return;
        };
        let request = DownloadRequest {
            url,
            destination: directory.join(model.file_name()),
            target: RouteTarget::Service(ServiceTarget::TranslationModelDownload),
            class: HttpClientClass::ModelDownload,
            expected_size: Some(model.size_bytes()),
        };
        let id = DownloadId::new();
        self.tasks.borrow_mut().insert(
            key.clone(),
            TaskEntry {
                id,
                seen: false,
                last: None,
            },
        );
        self.sites.borrow_mut().insert(key, site);
        (self.downloads.add)(request, id);
    }

    /// Retries the failed download of `model` in `directory` from `site`.
    ///
    /// The same site continues the partial file the failed task left; another site discards that
    /// file first, so the model is transferred from the site the user chose rather than stitched
    /// together from bytes of the two.
    pub(crate) fn retry(
        &self,
        app: &mut App,
        model: TranslationModel,
        directory: PathBuf,
        site: DownloadSite,
    ) {
        // A delete that is still in flight is already the retry's first half: asking it to delete
        // again would keep a restart that nothing would ever run.
        if self.row_state(model, &directory).is_removing() {
            return;
        }

        let key = (model, directory.clone());
        let previous = self
            .sites
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or(DownloadSite::HuggingFace);

        if previous == site {
            // The retry defaults to the site of the failed task, so an unchanged choice continues
            // what the task already has. A task the manager no longer holds falls back to a
            // download from the same site.
            self.sites.borrow_mut().insert(key, site);
            if self.resume(model, directory.clone()) {
                return;
            }
            self.download(model, directory, site);
            return;
        }

        self.pending_restarts.borrow_mut().insert(key, site);
        self.delete(app, model, directory);
    }

    /// Stops the download of `model` in `directory`, keeping its partial file.
    ///
    /// Returns whether the manager has a task to stop; a row without one has nothing to pause.
    pub(crate) fn pause(&self, model: TranslationModel, directory: PathBuf) -> bool {
        let Some(entry) = self.tasks.borrow().get(&(model, directory)).cloned() else {
            return false;
        };
        (self.downloads.pause)(entry.id);
        true
    }

    /// Continues the paused or failed download of `model` in `directory` from its partial file.
    ///
    /// Returns whether the manager has a task to continue.
    pub(crate) fn resume(&self, model: TranslationModel, directory: PathBuf) -> bool {
        let Some(entry) = self.tasks.borrow().get(&(model, directory)).cloned() else {
            return false;
        };
        (self.downloads.resume)(entry.id);
        true
    }

    /// Deletes `model` from `directory`, unloading it before the file goes.
    pub(crate) fn delete(&self, app: &mut App, model: TranslationModel, directory: PathBuf) {
        self.remove(app, model, directory, Removal::Delete);
    }

    /// Stops the download of `model` in `directory` for good and removes what it wrote.
    ///
    /// A stopped download is one the user does not want: its partial file is discarded rather
    /// than kept for a resume, so the row goes back to offering a download.
    pub(crate) fn terminate(&self, app: &mut App, model: TranslationModel, directory: PathBuf) {
        self.remove(app, model, directory, Removal::Terminate);
    }

    /// Removes what `model` has in `directory`, unloading it before the files go.
    fn remove(&self, app: &mut App, model: TranslationModel, directory: PathBuf, removal: Removal) {
        if self.row_state(model, &directory).is_removing() {
            return;
        }

        let clears_selection = self.store.read(app, |config| {
            config.translation.model_directory.as_ref() == Some(&directory)
                && config.translation.selected_model == Some(model)
        });
        if clears_selection {
            self.write_change(
                app,
                SettingsChange::TranslationModelSelected { model: None },
            );
        }

        let key = (model, directory.clone());
        let path = directory.join(model.file_name());
        // A stopped transfer leaves its bytes in the partial file beside the destination, so a
        // row without a task can still own files this removal has to remove.
        let files_exist = path.exists()
            || download_provider::partial::path(&path).is_some_and(|part| part.exists());
        let task = self.tasks.borrow().get(&key).map(|entry| entry.id);
        self.sites.borrow_mut().remove(&key);
        self.removals.borrow_mut().insert(key.clone(), removal);
        self.states
            .borrow_mut()
            .insert(key.clone(), removal.running());

        match task {
            // A file in place is unloaded before the manager removes it, so the engine cannot be
            // holding it open while the removal runs.
            Some(id) if path.exists() => {
                self.spawn_unload(key, removal, PendingKind::UnloadTask(id), path);
            }
            // A running, paused or failed download has no destination file to unload.
            Some(id) => (self.downloads.delete)(id),
            // Files this process did not download have no task, only the files to remove.
            None if files_exist => {
                self.spawn_unload(key, removal, PendingKind::Files, path);
            }
            None => {
                self.finish_removal(app, key, removal, Ok(()));
            }
        }
    }

    /// Runs the engine unload, and the file removal when there is no task to do it, on the
    /// runtime's blocking pool.
    fn spawn_unload(&self, key: RowKey, removal: Removal, kind: PendingKind, path: PathBuf) {
        let runtime = self.runtime.clone();
        let worker_runtime = runtime.clone();
        let unload = Arc::clone(&self.unload);
        let timeout = self.delete_timeout.get();
        let completions = Arc::clone(&self.completions);
        let wake = self.wake.clone();
        let remove_files = kind == PendingKind::Files;
        drop(runtime.spawn_blocking(move || {
            let result = unload_and_remove(&worker_runtime, unload, path, timeout, remove_files);
            completions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(PendingOutcome {
                    key,
                    removal,
                    kind,
                    result,
                });
            wake.wake();
        }));
    }

    /// Applies what a blocking removal worker produced.
    fn apply_outcome(&self, app: &mut App, outcome: PendingOutcome) -> bool {
        match outcome.kind {
            PendingKind::Files => {
                self.finish_removal(app, outcome.key, outcome.removal, outcome.result)
            }
            PendingKind::UnloadTask(id) => match outcome.result {
                // The engine released the file: the manager can remove the task and its files now.
                Ok(()) => {
                    (self.downloads.delete)(id);
                    false
                }
                Err(failure) => {
                    log::warn!(
                        "failed to unload translation model {:?} for removal: {failure:?}",
                        outcome.key.0
                    );
                    self.finish_removal(app, outcome.key, outcome.removal, Err(failure))
                }
            },
        }
    }

    /// Applies what a removal produced.
    fn finish_removal(
        &self,
        app: &mut App,
        key: RowKey,
        removal: Removal,
        result: Result<(), DeleteFailure>,
    ) -> bool {
        let outcome = match result {
            Ok(()) => Ok(()),
            // The file is already gone, which is what the removal asked for.
            Err(DeleteFailure::Io {
                kind: io::ErrorKind::NotFound,
            }) => Ok(()),
            Err(failure) => Err(failure),
        };

        match outcome {
            Ok(()) => {
                let (model, directory) = key.clone();
                self.removals.borrow_mut().remove(&key);
                self.forget(&key);
                #[cfg(test)]
                self.forced_downloaded.borrow_mut().remove(&key);
                // A model chosen while its removal was already running was not there when the
                // removal started to look for it, so the selection is cleared again here: by now
                // the files are gone, and a selection naming them is one the page cannot load.
                let clears_selection = self.store.read(app, |config| {
                    config.translation.model_directory.as_ref() == Some(&directory)
                        && config.translation.selected_model == Some(model)
                });
                if clears_selection {
                    self.write_change(
                        app,
                        SettingsChange::TranslationModelSelected { model: None },
                    );
                }

                // A retry that chose another site is a removal and then a download, started only
                // once the files the removal owned are gone.
                let restart = self.pending_restarts.borrow_mut().remove(&key);
                if let Some(site) = restart {
                    self.download(model, directory, site);
                }
                true
            }
            Err(failure) => {
                // The retry's intent is not kept for later: a removal that failed is what the
                // row reports, and the user decides whether to try again.
                self.removals.borrow_mut().remove(&key);
                self.pending_restarts.borrow_mut().remove(&key);
                self.states
                    .borrow_mut()
                    .insert(key, removal.failed(failure));
                true
            }
        }
    }

    /// Records a download that finished, selecting it when nothing usable is selected.
    ///
    /// The row releases its task here: the model's file is in place, so asking for it again starts
    /// a new task instead of finding a completed one, and deleting the downloaded file takes the
    /// direct path that needs no task.
    fn complete_download(&self, app: &mut App, key: &RowKey) -> bool {
        let (model, directory) = key.clone();
        self.forget(key);
        let auto_select = self.store.read(app, |config| {
            config.translation.model_directory.as_ref() == Some(&directory)
                && !config
                    .translation
                    .selected_model
                    .is_some_and(|selected| self.is_downloaded(selected, &directory))
        });
        if auto_select {
            self.write_change(
                app,
                SettingsChange::TranslationModelSelected { model: Some(model) },
            );
        }

        true
    }

    /// Applies one task snapshot to the row it belongs to.
    fn apply_status(
        &self,
        app: &mut App,
        key: &RowKey,
        previous: Option<&DownloadStatus>,
        status: &DownloadStatus,
    ) -> bool {
        // A removal owns its row: a snapshot of the transfer it is stopping is stale, and letting
        // it through would take the row out of the removal it is drawn in.
        let removal = self.removals.borrow().get(key).copied();
        if let Some(removal) = removal {
            return match status {
                DownloadStatus::Deleting => self.set_state(key.clone(), removal.running()),
                // Only a failure the delete itself produced is the removal's failure: a task that
                // was already failed when the row was deleted stays the download failure it was.
                DownloadStatus::Failed { failure }
                    if matches!(previous, Some(DownloadStatus::Deleting)) =>
                {
                    self.finish_removal(app, key.clone(), removal, Err(delete_failure(*failure)))
                }
                _ => false,
            };
        }

        let current = self.row_state(key.0, &key.1);
        let next = match status {
            DownloadStatus::Downloading {
                downloaded,
                mb_per_second,
                ..
            } => DownloadState::Downloading {
                downloaded: *downloaded,
                mb_per_second: *mb_per_second,
            },
            DownloadStatus::Pausing { downloaded, .. } => DownloadState::Pausing {
                downloaded: *downloaded,
            },
            DownloadStatus::Paused { downloaded, .. } => DownloadState::Paused {
                downloaded: *downloaded,
            },
            // The row already knows which removal it asked for; a delete this manager did not
            // start is drawn as the deletion it is.
            DownloadStatus::Deleting => current
                .removal()
                .map_or(DownloadState::Deleting, Removal::running),
            DownloadStatus::Completed { .. } => {
                return self.complete_download(app, key);
            }
            DownloadStatus::Failed { failure } => match current.removal() {
                Some(removal) => removal.failed(delete_failure(*failure)),
                // A removal that failed stays reported as that failure until the user asks
                // again: the manager keeps publishing the failed status it settled in.
                None if current.is_failed_removal() => return false,
                None => DownloadState::DownloadFailed { failure: *failure },
            },
        };
        self.set_state(key.clone(), next)
    }

    /// Applies the disappearance of a row's task from the manager's snapshot.
    fn task_vanished(&self, app: &mut App, key: &RowKey) -> bool {
        match self.row_state(key.0, &key.1).removal() {
            Some(removal) => self.finish_removal(app, key.clone(), removal, Ok(())),
            None => {
                self.forget(key);
                true
            }
        }
    }

    /// The outcomes the blocking workers produced since the last frame.
    fn take_outcomes(&self) -> Vec<PendingOutcome> {
        std::mem::take(
            &mut *self
                .completions
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Forgets everything one row's manager knew, except whether a test forced it downloaded.
    fn forget(&self, key: &RowKey) {
        self.states.borrow_mut().remove(key);
        self.tasks.borrow_mut().remove(key);
        self.sites.borrow_mut().remove(key);
    }

    /// Records one row's state, answering whether it was a change.
    fn set_state(&self, key: RowKey, state: DownloadState) -> bool {
        let mut states = self.states.borrow_mut();
        if states.get(&key) == Some(&state) {
            return false;
        }
        states.insert(key, state);
        true
    }

    /// Marks one row's task as one whose snapshot the manager published.
    fn mark_seen(&self, key: &RowKey) {
        if let Some(entry) = self.tasks.borrow_mut().get_mut(key) {
            entry.seen = true;
        }
    }

    /// Records the status one row's task published and answers the one before it.
    fn observe_status(&self, key: &RowKey, status: &DownloadStatus) -> Option<DownloadStatus> {
        let mut tasks = self.tasks.borrow_mut();
        let entry = tasks.get_mut(key)?;
        let previous = entry.last.clone();
        entry.last = Some(status.clone());
        previous
    }

    /// Writes one change into the settings through the session the composition root injected.
    fn write_change(&self, app: &mut App, change: SettingsChange) {
        let window = first_window(app);
        let Some(window) = window else {
            log::warn!("the translation downloads found no window to write the selection through");
            return;
        };
        let _ = self.store.write(app, window, change);
    }

    /// Treats `model` as downloaded without creating a multi-gigabyte file.
    #[cfg(test)]
    pub(crate) fn force_downloaded_for_test(&self, model: TranslationModel, directory: PathBuf) {
        self.forced_downloaded
            .borrow_mut()
            .insert((model, directory));
    }

    /// Stops a test treating `model` as downloaded, as removing the file outside the application
    /// would.
    #[cfg(test)]
    pub(crate) fn forget_downloaded_for_test(&self, model: TranslationModel, directory: PathBuf) {
        self.forced_downloaded
            .borrow_mut()
            .remove(&(model, directory));
    }

    /// Puts one row into a state a test draws without a network.
    #[cfg(test)]
    pub(crate) fn set_state_for_test(
        &self,
        model: TranslationModel,
        directory: PathBuf,
        state: DownloadState,
    ) {
        self.states.borrow_mut().insert((model, directory), state);
    }

    /// Shortens the wait a removal allows its engine unload, so a test can reach the timeout.
    #[cfg(test)]
    pub(crate) fn set_delete_timeout_for_test(&self, timeout: Duration) {
        self.delete_timeout.set(timeout);
    }
}

/// Whether a file is present at the size the model publishes.
pub(crate) fn file_size_matches(path: &Path, expected: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected)
}

/// The window a settings write repaints, when the application has one.
fn first_window(app: &App) -> Option<WindowId> {
    app.windows().next().map(|(window, _)| window)
}

/// Unloads the engine, enforces the unload deadline, and removes the file when asked to.
///
/// The unload runs on a blocking worker of its own so a hung engine cannot pin the removal: the
/// deadline here is what reports [`DeleteFailure::TimedOut`] instead.
fn unload_and_remove(
    runtime: &tokio::runtime::Handle,
    unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
    path: PathBuf,
    timeout: Duration,
    remove_files: bool,
) -> Result<(), DeleteFailure> {
    let completion: Arc<Mutex<Option<Result<(), UnloadFailure>>>> = Arc::new(Mutex::new(None));
    let worker_completion = Arc::clone(&completion);
    let worker_path = path.clone();
    drop(runtime.spawn_blocking(move || {
        *worker_completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(unload(&worker_path));
    }));

    let deadline = Instant::now() + timeout;
    let unload_result = loop {
        if let Some(result) = completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            break result;
        }
        if Instant::now() >= deadline {
            return Err(DeleteFailure::TimedOut);
        }
        std::thread::sleep(DELETE_POLL_INTERVAL);
    };
    unload_result.map_err(DeleteFailure::Unload)?;

    if !remove_files {
        return Ok(());
    }
    if let Some(part) = download_provider::partial::path(&path) {
        // Best effort: a missing or locked partial file must not stop the model itself from being
        // removed.
        let _ = std::fs::remove_file(part);
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DeleteFailure::Io {
            kind: source.kind(),
        }),
    }
}

/// Why the download manager could not delete a model's files, in the page's own vocabulary.
fn delete_failure(failure: DownloadFailure) -> DeleteFailure {
    match failure {
        DownloadFailure::Io {
            kind: io::ErrorKind::TimedOut,
        } => DeleteFailure::TimedOut,
        DownloadFailure::Io { kind } => DeleteFailure::Io { kind },
        failure => DeleteFailure::Provider(failure),
    }
}

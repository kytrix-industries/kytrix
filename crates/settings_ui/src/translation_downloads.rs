//! The process-wide translation model downloads.
//!
//! A download belongs to the process rather than to a settings session: closing the settings
//! page must not cancel a model that is still arriving, and opening the page again must show
//! the progress the download has reached. The entity is therefore installed as a global by the
//! composition root, and the settings page only observes the entity it finds there.
//!
//! The transfers themselves belong to the [`DownloadProvider`]: this module asks it to add,
//! pause, resume, stop and delete the task of one model in one directory, and draws the states
//! it publishes. What stays here is the translation model's own meaning — which file of which
//! directory a row is, the engine unload that has to happen before a file goes, and the
//! directory and selection a finished download records.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ahash::AHashMap;
use app_config::{
    network::{RouteTarget, ServiceTarget},
    translation::{DownloadSite, TranslationModel},
};
use app_config_provider::manager::ConfigManager;
use download_provider::{
    error::DownloadFailure,
    provider::DownloadProvider,
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadStatus},
};
use gpui_kit::{
    App, AppContext as _, BackgroundExecutor, BorrowAppContext as _, Context, Entity, Global,
    Subscription,
};
use network::transport::http::HttpClientClass;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

/// How often a delete checks whether its blocking worker has finished.
const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(10);
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
    /// The process-wide manager every model download is a task of.
    pub provider: Entity<DownloadProvider>,
    /// The application runtime the engine unload and the file removal run on.
    pub runtime: Runtime,
    /// Drops the translation engine that may hold the model file, before it is deleted.
    pub unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
}

/// Installs the download manager for the process.
pub fn install(cx: &mut App, deps: TranslationDownloadsDeps) {
    let downloads = cx.new(|cx| TranslationDownloads::new(deps, cx));
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

/// The process-wide model downloads and what each of them last did.
pub(crate) struct TranslationDownloads {
    provider: Entity<DownloadProvider>,
    runtime: Runtime,
    unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
    /// The last outcome of each model in each directory, keyed by both.
    states: AHashMap<(TranslationModel, PathBuf), DownloadState>,
    /// The provider task of each model in each directory, which is what a pause, a resume or a
    /// delete of the row acts on.
    tasks: AHashMap<(TranslationModel, PathBuf), DownloadId>,
    /// The site each download was started from, which is what tells a retry whether the site
    /// the user chose continues that task or asks for a different transfer.
    sites: AHashMap<(TranslationModel, PathBuf), DownloadSite>,
    /// The downloads a retry asked to start once the files of another site are gone.
    pending_restarts: AHashMap<(TranslationModel, PathBuf), DownloadSite>,
    /// Models a test treats as downloaded without creating a multi-gigabyte file.
    #[cfg(test)]
    forced_downloaded: ahash::AHashSet<(TranslationModel, PathBuf)>,
    /// Whether a test asked for transfers to be recorded rather than started.
    #[cfg(test)]
    transfers_deferred: bool,
    /// Held because dropping it would stop the manager from following its downloads.
    _provider_events: Subscription,
}

impl TranslationDownloads {
    pub(crate) fn new(deps: TranslationDownloadsDeps, cx: &mut Context<Self>) -> Self {
        let capacity = TranslationModel::ALL.len();
        let provider_events = cx.subscribe(&deps.provider, |this, _, event: &DownloadEvent, cx| {
            this.provider_event(event, cx);
        });

        Self {
            provider: deps.provider,
            runtime: deps.runtime,
            unload: deps.unload,
            states: AHashMap::with_capacity(capacity),
            tasks: AHashMap::with_capacity(capacity),
            sites: AHashMap::with_capacity(capacity),
            pending_restarts: AHashMap::with_capacity(capacity),
            #[cfg(test)]
            forced_downloaded: ahash::AHashSet::with_capacity(capacity),
            #[cfg(test)]
            transfers_deferred: false,
            _provider_events: provider_events,
        }
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

    /// The site the download of `model` in `directory` was started from, for the tests that
    /// pin what a retry asks the manager to do.
    #[cfg(test)]
    pub(crate) fn site(&self, model: TranslationModel, directory: &Path) -> Option<DownloadSite> {
        self.sites.get(&(model, directory.to_path_buf())).copied()
    }

    /// Starts downloading `model` into `directory` from `site`.
    ///
    /// This is what both a first download and the restart a retry asks for go through. A new
    /// transfer starts from the chosen site; a paused or failed task is continued only when the
    /// site it was started from is unknown or the same, because continuing it would report the
    /// bytes of one site under another.
    pub(crate) fn download(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        site: DownloadSite,
        cx: &mut Context<Self>,
    ) {
        let key = (model, directory.clone());
        // A task that already exists from another site is left alone: resuming it, or recording
        // this site over it, would report the bytes of one site under another.
        if self.tasks.contains_key(&key)
            && self
                .sites
                .get(&key)
                .is_some_and(|recorded| *recorded != site)
        {
            return;
        }
        // A delete that is still in flight owns the files: a download started now would truncate
        // the partial file that delete is removing.
        if self.pending_restarts.contains_key(&key)
            || self.is_downloaded(model, &directory)
            || self.row_state(model, &directory).is_removing()
        {
            return;
        }

        let Ok(url) = Url::parse(model.download_url(site)) else {
            self.states.insert(
                key,
                DownloadState::DownloadFailed {
                    failure: DownloadFailure::InvalidUrl,
                },
            );
            cx.notify();
            return;
        };

        #[cfg(test)]
        if self.transfers_deferred {
            self.sites.insert(key.clone(), site);
            self.states.insert(
                key,
                DownloadState::Downloading {
                    downloaded: 0,
                    mb_per_second: 0.0,
                },
            );
            cx.notify();
            return;
        }

        // The site is recorded only when this call is what starts the transfer. A call that
        // finds a paused or failed task continues that task, whose bytes are from the site it
        // was started from: recording the site asked for here would report bytes of one site
        // under the name of another.
        if self.start_download(model, directory, url, cx) {
            self.sites.insert(key, site);
        }
    }

    /// Retries the failed download of `model` in `directory` from `site`.
    ///
    /// The same site continues the partial file the failed task left; another site discards that
    /// file first, so the model is transferred from the site the user chose rather than stitched
    /// together from bytes of the two.
    pub(crate) fn retry(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        site: DownloadSite,
        cx: &mut Context<Self>,
    ) {
        // A delete that is still in flight is already the retry's first half: asking it to
        // delete again would keep a restart that nothing would ever run.
        if self.row_state(model, &directory).is_removing() {
            return;
        }

        let key = (model, directory.clone());
        let previous = self
            .sites
            .get(&key)
            .copied()
            .unwrap_or(DownloadSite::HuggingFace);

        if previous == site {
            // The retry defaults to the site of the failed task, so an unchanged choice continues
            // what the task already has. A task the provider no longer holds falls back to a
            // download from the same site.
            self.sites.insert(key, site);
            if self.resume(model, directory.clone(), cx) {
                return;
            }
            self.download(model, directory, site, cx);
            return;
        }

        self.pending_restarts.insert(key, site);
        self.delete(model, directory, cx);
    }

    /// Starts downloading `model` into `directory` from `url`, or resumes the download already
    /// there.
    ///
    /// Returns whether a new provider task was added; a paused or failed task it continued
    /// keeps the site it was started from.
    fn start_download(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        url: Url,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = (model, directory.clone());
        if self.pending_restarts.contains_key(&key)
            || self.is_downloaded(model, &directory)
            || self.row_state(model, &directory).is_removing()
        {
            return false;
        }

        if let Some(id) = self.tasks.get(&key).copied() {
            // The manager's own status decides: a paused or failed task continues from its
            // partial file, while anything else is already doing what the row asked for. A task
            // the provider no longer holds is forgotten, so the download starts afresh.
            match self.provider.read(cx).task(id) {
                Some(task)
                    if matches!(
                        task.status,
                        DownloadStatus::Paused { .. } | DownloadStatus::Failed { .. }
                    ) =>
                {
                    self.provider
                        .update(cx, |provider, cx| provider.resume(id, cx));
                    return false;
                }
                Some(_) => return false,
                None => {
                    self.tasks.remove(&key);
                }
            }
        }

        self.start_new_task(model, directory, url, cx);
        true
    }

    /// Adds the provider task one download is.
    fn start_new_task(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        url: Url,
        cx: &mut Context<Self>,
    ) {
        let request = DownloadRequest {
            url,
            destination: directory.join(model.file_name()),
            target: RouteTarget::Service(ServiceTarget::TranslationModelDownload),
            class: HttpClientClass::ModelDownload,
            expected_size: Some(model.size_bytes()),
        };
        let id = self
            .provider
            .update(cx, |provider, cx| provider.add(request, cx));
        self.tasks.insert((model, directory), id);
        // The provider publishes the task while it is being added, before this row knows the
        // task is its own; adopting its status here is what tells the row it is running.
        let task = self.provider.read(cx).task(id);
        if let Some(task) = task {
            self.provider_event(&DownloadEvent::TaskUpdated(task), cx);
        }
    }

    /// Stops the download of `model` in `directory`, keeping its partial file.
    ///
    /// Returns whether the download manager accepted the pause; a row without a running task
    /// has nothing to pause.
    pub(crate) fn pause(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.tasks.get(&(model, directory)).copied() else {
            return false;
        };
        self.provider
            .update(cx, |provider, cx| provider.pause(id, cx))
    }

    /// Continues the paused or failed download of `model` in `directory` from its partial file.
    ///
    /// Returns whether the download manager accepted the resume.
    pub(crate) fn resume(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = (model, directory);
        #[cfg(test)]
        if self.transfers_deferred {
            self.states.insert(
                key,
                DownloadState::Downloading {
                    downloaded: 0,
                    mb_per_second: 0.0,
                },
            );
            cx.notify();
            return true;
        }

        let Some(id) = self.tasks.get(&key).copied() else {
            return false;
        };
        self.provider
            .update(cx, |provider, cx| provider.resume(id, cx))
    }

    /// Applies a download event the provider published.
    fn provider_event(&mut self, event: &DownloadEvent, cx: &mut Context<Self>) {
        match event {
            DownloadEvent::TaskUpdated(task) => {
                let Some(key) = self.task_key(task.id) else {
                    return;
                };
                let state = match &task.status {
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
                    DownloadStatus::Deleting => {
                        // The row already knows which removal it asked for; a delete this
                        // manager did not start is drawn as the deletion it is.
                        self.states
                            .get(&key)
                            .and_then(DownloadState::removal)
                            .map_or(DownloadState::Deleting, Removal::running)
                    }
                    DownloadStatus::Completed { .. } => {
                        self.complete_download(key.0, key.1, cx);
                        return;
                    }
                    DownloadStatus::Failed { failure } => {
                        match self.states.get(&key).and_then(DownloadState::removal) {
                            Some(removal) => removal.failed(delete_failure(*failure)),
                            None => DownloadState::DownloadFailed { failure: *failure },
                        }
                    }
                };
                self.states.insert(key, state);
                cx.notify();
            }
            DownloadEvent::TaskRemoved(id) => {
                let Some(key) = self.task_key(*id) else {
                    return;
                };
                self.tasks.remove(&key);
                match self.states.get(&key).and_then(DownloadState::removal) {
                    Some(removal) => self.finish_removal(key.0, key.1, removal, Ok(()), cx),
                    None => {
                        self.states.remove(&key);
                        cx.notify();
                    }
                }
            }
        }
    }

    /// The model and directory whose row a provider task is.
    fn task_key(&self, id: DownloadId) -> Option<(TranslationModel, PathBuf)> {
        self.tasks
            .iter()
            .find(|(_, task)| **task == id)
            .map(|(key, _)| key.clone())
    }

    /// Records a download that finished, selecting it when nothing usable is selected.
    ///
    /// The row releases its task here: the model's file is in place, so asking for it again
    /// starts a new task instead of finding a completed one, and deleting the downloaded file
    /// takes the direct path that needs no task. The provider keeps its own completed record,
    /// which no caller refers to any more.
    fn complete_download(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let key = (model, directory.clone());
        self.states.remove(&key);
        self.tasks.remove(&key);
        self.sites.remove(&key);

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

        cx.notify();
    }

    /// Deletes `model` from `directory`, unloading it before the file goes.
    pub(crate) fn delete(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.remove(model, directory, Removal::Delete, cx);
    }

    /// Stops the download of `model` in `directory` for good and removes what it wrote.
    ///
    /// A stopped download is one the user does not want: its partial file is discarded rather
    /// than kept for a resume, so the row goes back to offering a download.
    pub(crate) fn terminate(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.remove(model, directory, Removal::Terminate, cx);
    }

    /// Removes what `model` has in `directory`, unloading it before the files go.
    fn remove(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        removal: Removal,
        cx: &mut Context<Self>,
    ) {
        if self.row_state(model, &directory).is_removing() {
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

        let key = (model, directory.clone());
        let path = directory.join(model.file_name());
        // A stopped transfer leaves its bytes in the partial file beside the destination, so a
        // row without a task can still own files this removal has to remove.
        let files_exist = path.exists()
            || download_provider::partial::path(&path).is_some_and(|part| part.exists());
        let task = self.tasks.get(&key).copied();
        self.sites.remove(&key);
        self.states.insert(key, removal.running());
        cx.notify();

        match task {
            // A file in place is unloaded before the manager removes it, so the engine cannot be
            // holding it open while the removal runs. A stopped transfer may have left one too.
            Some(id) if path.exists() => {
                self.unload_then_remove(id, model, directory, path, removal, cx);
            }
            // A running, paused or failed download has no destination file to unload.
            Some(id) => self.remove_provider_task(id, model, directory, removal, cx),
            // Files this process did not download have no task, only the files to remove.
            None if files_exist => self.remove_files(model, directory, removal, cx),
            None => self.finish_removal(model, directory, removal, Ok(()), cx),
        }
    }

    /// Unloads the engine and then lets the provider remove the task and its files.
    fn unload_then_remove(
        &mut self,
        id: DownloadId,
        model: TranslationModel,
        directory: PathBuf,
        path: PathBuf,
        removal: Removal,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.runtime.clone();
        let unload = Arc::clone(&self.unload);
        let completion = Arc::new(Mutex::new(None));
        let worker_completion = Arc::clone(&completion);
        cx.spawn(async move |downloads, cx| {
            drop(runtime.blocking(move || {
                *worker_completion
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    Some(unload(&path).map_err(DeleteFailure::Unload));
            }));
            let clock = cx.background_executor().clone();
            let result = wait_for_delete(&completion, &clock).await;
            let _ = downloads.update(cx, |downloads, cx| {
                downloads.finish_unload(id, model, directory, removal, result, cx)
            });
        })
        .detach();
    }

    /// Applies what an unload for a removal produced.
    fn finish_unload(
        &mut self,
        id: DownloadId,
        model: TranslationModel,
        directory: PathBuf,
        removal: Removal,
        result: Result<(), DeleteFailure>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => self.remove_provider_task(id, model, directory, removal, cx),
            Err(failure) => {
                log::warn!("failed to unload translation model {model:?} for removal: {failure:?}");
                self.finish_removal(model, directory, removal, Err(failure), cx);
            }
        }
    }

    /// Asks the provider to remove a task, falling back to the files when it no longer has it.
    fn remove_provider_task(
        &mut self,
        id: DownloadId,
        model: TranslationModel,
        directory: PathBuf,
        removal: Removal,
        cx: &mut Context<Self>,
    ) {
        if !self
            .provider
            .update(cx, |provider, cx| provider.delete(id, cx))
        {
            self.tasks.remove(&(model, directory.clone()));
            self.remove_files(model, directory, removal, cx);
        }
    }

    /// Unloads the engine and removes the model's files without a provider task.
    fn remove_files(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        removal: Removal,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.runtime.clone();
        let unload = Arc::clone(&self.unload);
        let completion = Arc::new(Mutex::new(None));
        let worker_completion = Arc::clone(&completion);
        let path = directory.join(model.file_name());
        cx.spawn(async move |downloads, cx| {
            drop(runtime.blocking(move || {
                let result = (|| {
                    unload(&path).map_err(DeleteFailure::Unload)?;
                    if let Some(part) = download_provider::partial::path(&path) {
                        // Best effort: a missing or locked partial file must not stop the
                        // model itself from being removed.
                        let _ = std::fs::remove_file(part);
                    }
                    match std::fs::remove_file(&path) {
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
            let clock = cx.background_executor().clone();
            let result = wait_for_delete(&completion, &clock).await;
            let _ = downloads.update(cx, |downloads, cx| {
                if let Err(error) = &result {
                    log::warn!("failed to remove translation model {model:?}: {error:?}");
                }
                downloads.finish_removal(model, directory, removal, result, cx)
            });
        })
        .detach();
    }

    /// Applies what a removal produced.
    fn finish_removal(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        removal: Removal,
        result: Result<(), DeleteFailure>,
        cx: &mut Context<Self>,
    ) {
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
                let key = (model, directory.clone());
                self.states.remove(&key);
                self.sites.remove(&key);
                #[cfg(test)]
                self.forced_downloaded.remove(&key);
                // A model chosen while its removal was already running was not there when the
                // removal started to look for it, so the selection is cleared again here: by now
                // the files are gone, and a selection naming them is one the page cannot load.
                // A selection is only this model's to clear when it names the directory the
                // removal emptied.
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

                // A retry that chose another site is a removal and then a download. The provider
                // is in the middle of publishing the end of the removal here, so the restart is
                // deferred out of that update rather than re-entering it, and it waits until the
                // files the removal owned are gone before a new transfer can truncate them.
                if let Some(site) = self.pending_restarts.remove(&key) {
                    let downloads = cx.entity().downgrade();
                    cx.defer(move |cx| {
                        let _ = downloads.update(cx, |downloads, cx| {
                            downloads.download(model, directory, site, cx);
                        });
                    });
                }
            }
            Err(failure) => {
                // The retry's intent is not kept for later: a removal that failed is what the
                // row reports, and the user is the one who decides whether to try again.
                self.pending_restarts.remove(&(model, directory.clone()));
                self.states
                    .insert((model, directory), removal.failed(failure));
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

    /// Records a download a test started without transferring it, and makes every download this
    /// manager is asked to start afterwards record what it was asked for rather than transfer it.
    #[cfg(test)]
    pub(crate) fn set_transfers_deferred_for_test(
        &mut self,
        model: TranslationModel,
        directory: PathBuf,
        site: DownloadSite,
        cx: &mut Context<Self>,
    ) {
        self.transfers_deferred = true;
        self.sites.insert((model, directory.clone()), site);
        self.states.insert(
            (model, directory),
            DownloadState::Downloading {
                downloaded: 0,
                mb_per_second: 0.0,
            },
        );
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

/// Whether `path` is a file of exactly `expected` bytes.
fn file_size_matches(path: &Path, expected: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected)
}

/// Waits for a blocking delete worker to report, or reports that it did not finish in time.
async fn wait_for_delete(
    completion: &Mutex<Option<Result<(), DeleteFailure>>>,
    clock: &BackgroundExecutor,
) -> Result<(), DeleteFailure> {
    let deadline = clock.now() + DELETE_TIMEOUT;
    loop {
        if let Some(result) = completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return result;
        }
        if clock.now() >= deadline {
            return Err(DeleteFailure::TimedOut);
        }
        clock.timer(DELETE_POLL_INTERVAL).await;
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        fs, io,
        net::{Ipv4Addr, SocketAddr},
        path::Path,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
        time::{Duration, Instant},
    };

    use app_config::{
        network::{RouteTarget, ServiceTarget},
        settings::Config,
        translation::{DownloadSite, TranslationModel},
    };
    use app_config_provider::manager::ConfigManager;
    use download_provider::{
        global::{DownloadProviderDeps, install as install_provider},
        provider::DownloadProvider,
        request::DownloadRequest,
        task::{DownloadEvent, DownloadId, DownloadStatus, DownloadTask},
    };
    use gpui_kit::{AppContext as _, BorrowAppContext as _, Entity, TestAppContext};
    use hyper::{
        Request, Response, StatusCode,
        body::{Body, Bytes, Frame, Incoming, SizeHint},
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
    use widget_tokio_runtime::runtime::Runtime;

    use super::{
        DELETE_TIMEOUT, DeleteFailure, DownloadFailure, DownloadState, Removal,
        TranslationDownloads, TranslationDownloadsDeps, UnloadFailure, file_size_matches,
    };

    /// How much of a paced payload each frame carries.
    const FRAME: usize = 16 * 1024;
    /// How long a paced server waits between frames.
    const PACE: Duration = Duration::from_millis(25);
    /// How long a test waits for an asynchronous outcome before failing.
    const WAIT_LIMIT: Duration = Duration::from_secs(10);

    /// The bytes a test downloads, different at every offset so a misplaced resume is visible.
    fn payload(length: usize) -> Bytes {
        Bytes::from(
            (0..length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<u8>>(),
        )
    }

    /// A response body the test server hands out one frame at a time.
    ///
    /// The pause between frames is what makes a transfer observably in flight; without it the
    /// whole payload reaches the kernel at once and there is nothing left to interrupt.
    struct TestBody {
        frames: VecDeque<Frame<Bytes>>,
        wait: Option<Pin<Box<Sleep>>>,
        pace: Duration,
        length: u64,
    }

    impl TestBody {
        fn new(bytes: Bytes, pace: Option<Duration>) -> Self {
            let length = bytes.len() as u64;
            match pace {
                Some(pace) => Self {
                    frames: bytes
                        .chunks(FRAME)
                        .map(|part| Frame::data(Bytes::copy_from_slice(part)))
                        .collect(),
                    wait: None,
                    pace,
                    length,
                },
                None => Self {
                    frames: if bytes.is_empty() {
                        VecDeque::new()
                    } else {
                        VecDeque::from([Frame::data(bytes)])
                    },
                    wait: None,
                    pace: Duration::ZERO,
                    length,
                },
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

    /// Starts a server answering every request with `payload`, paced when a pace is asked for.
    ///
    /// The accept loop runs on the shared test runtime for the rest of the process; a test only
    /// needs the address.
    fn spawn_server(payload: Bytes, pace: Option<Duration>) -> SocketAddr {
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
                let payload = payload.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |_: Request<Incoming>| {
                        let payload = payload.clone();
                        async move {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .body(TestBody::new(payload, pace))
                                    .expect("the response is well formed"),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        }));

        address
    }

    /// The manager over the process-wide provider, for one test application.
    struct Harness {
        downloads: Entity<TranslationDownloads>,
        provider: Entity<DownloadProvider>,
    }

    fn harness(cx: &mut TestAppContext) -> Harness {
        harness_with(cx, Config::default(), Arc::new(|_: &Path| Ok(())))
    }

    fn harness_with(
        cx: &mut TestAppContext,
        config: Config,
        unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
    ) -> Harness {
        let (downloads, provider) = cx.update(|cx| {
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
            let provider = install_provider(
                cx,
                DownloadProviderDeps {
                    transport: Arc::new(TransportHub::new(TransportPolicy {
                        allow_plaintext: true,
                        extra_root_certificates: Vec::new(),
                    })),
                    runtime: Runtime::for_test(),
                },
            );
            let downloads = cx.new(|cx| {
                TranslationDownloads::new(
                    TranslationDownloadsDeps {
                        provider: provider.clone(),
                        runtime: Runtime::for_test(),
                        unload,
                    },
                    cx,
                )
            });

            (downloads, provider)
        });

        Harness {
            downloads,
            provider,
        }
    }

    /// Starts a provider task for `model` in `directory` and makes it the row's own task.
    ///
    /// This is the part of a download the manager cannot do without the model's real published
    /// address: the provider publishes the task while it is added, before the row knows the task
    /// is its own, so the row adopts the status here the way the completion path would.
    ///
    /// Returns the provider task the row adopted.
    fn adopt(
        cx: &mut TestAppContext,
        harness: &Harness,
        model: TranslationModel,
        directory: &Path,
        address: SocketAddr,
    ) -> DownloadId {
        let request = DownloadRequest {
            url: Url::parse(&format!("http://{address}/model")).expect("the test URL is a URL"),
            destination: directory.join(model.file_name()),
            target: RouteTarget::Service(ServiceTarget::TranslationModelDownload),
            class: HttpClientClass::ModelDownload,
            expected_size: None,
        };
        let id = cx.update(|cx| {
            harness
                .provider
                .update(cx, |provider, cx| provider.add(request, cx))
        });
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.tasks.insert((model, directory.to_path_buf()), id);
                let task = downloads.provider.read(cx).task(id);
                if let Some(task) = task {
                    downloads.provider_event(&DownloadEvent::TaskUpdated(task), cx);
                }
            });
        });

        id
    }

    fn state(
        cx: &mut TestAppContext,
        downloads: &Entity<TranslationDownloads>,
        model: TranslationModel,
        directory: &Path,
    ) -> DownloadState {
        cx.update(|cx| downloads.read(cx).row_state(model, directory))
    }

    /// Polls `condition` on the interface executor until it holds, with real time passing so the
    /// transfer it observes can make progress.
    ///
    /// The step is a tenth of the interval the manager publishes progress at, so waiting for a
    /// report costs a few real milliseconds rather than the whole interval.
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
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits for a delete to settle and its file to be gone.
    fn wait_until_deleted(
        cx: &mut TestAppContext,
        downloads: &Entity<TranslationDownloads>,
        model: TranslationModel,
        directory: &Path,
        path: &Path,
    ) {
        wait_until(cx, "the delete to finish", |cx| {
            !path.exists() && state(cx, downloads, model, directory) == DownloadState::Idle
        });
    }

    /// Deleting a file that is already gone is what the row asked for, not a failure.
    #[gpui_kit::test]
    fn deleting_a_missing_file_should_succeed(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let path = directory.path().join(model.file_name());

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });

        assert!(!path.exists());
        assert_eq!(
            state(cx, &harness.downloads, model, directory.path()),
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
        let harness = harness_with(
            cx,
            Config::default(),
            Arc::new(move |path: &Path| {
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
            }),
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until_deleted(cx, &harness.downloads, model, directory.path(), &path);

        assert_eq!(
            *order.lock().expect("the order log is not poisoned"),
            ["unload-before-remove"],
            "the engine must release the file before the delete removes it"
        );
    }

    /// A selection that was made while the removal was already running is cleared when the
    /// removal finishes: the files it names are gone by then.
    #[gpui_kit::test]
    fn finish_removal_should_clear_a_selection_made_while_the_removal_ran(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let mut config = Config::default();
        config.translation.model_directory = Some(directory.path().to_path_buf());
        let harness = harness_with(cx, config, Arc::new(|_: &Path| Ok(())));

        // What a stale menu choice or another writer may have stored after the removal started:
        // the selection was not there when the removal began to look for it.
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.translation.selected_model = Some(model), cx);
            });
        });

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.finish_removal(
                    model,
                    directory.path().to_path_buf(),
                    Removal::Delete,
                    Ok(()),
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|cx| cx
                .global::<ConfigManager>()
                .read()
                .translation
                .selected_model),
            None,
            "a selection that still names the removed model is cleared when the removal finishes"
        );
    }

    /// A finished removal only clears the selection when it names the directory the removal
    /// emptied: the same model in another directory is still there.
    #[gpui_kit::test]
    fn finish_removal_should_keep_a_selection_in_another_directory(cx: &mut TestAppContext) {
        let removed = TempDir::new().expect("the removed directory builds");
        let chosen = TempDir::new().expect("the chosen directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let mut config = Config::default();
        config.translation.model_directory = Some(chosen.path().to_path_buf());
        config.translation.selected_model = Some(model);
        let harness = harness_with(cx, config, Arc::new(|_: &Path| Ok(())));

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.finish_removal(
                    model,
                    removed.path().to_path_buf(),
                    Removal::Delete,
                    Ok(()),
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|cx| cx
                .global::<ConfigManager>()
                .read()
                .translation
                .selected_model),
            Some(model),
            "the selection names a directory the removal did not empty, so it is kept"
        );
    }

    /// A finished delete that found the file already gone is still success.
    #[gpui_kit::test]
    fn finish_removal_should_accept_a_missing_file(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.finish_removal(
                    model,
                    directory.path().to_path_buf(),
                    Removal::Delete,
                    Err(DeleteFailure::Io {
                        kind: io::ErrorKind::NotFound,
                    }),
                    cx,
                );
            });
        });

        assert_eq!(
            state(cx, &harness.downloads, model, directory.path()),
            DownloadState::Idle
        );
    }

    /// A failed unload leaves the row reporting the reason instead of hanging in Deleting.
    #[gpui_kit::test]
    fn finish_removal_should_report_an_unload_failure(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.finish_removal(
                    model,
                    directory.path().to_path_buf(),
                    Removal::Delete,
                    Err(DeleteFailure::Unload(UnloadFailure::TimedOut)),
                    cx,
                );
            });
        });

        assert_eq!(
            state(cx, &harness.downloads, model, directory.path()),
            DownloadState::DeleteFailed {
                failure: DeleteFailure::Unload(UnloadFailure::TimedOut),
            },
            "an unload failure should become DeleteFailed"
        );
    }

    /// A terminate that fails to unload reports it as the failed stop it is, not as a delete.
    #[gpui_kit::test]
    fn a_terminate_should_report_its_own_failure(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.finish_removal(
                    model,
                    directory.path().to_path_buf(),
                    Removal::Terminate,
                    Err(DeleteFailure::Unload(UnloadFailure::Engine)),
                    cx,
                );
            });
        });

        assert_eq!(
            state(cx, &harness.downloads, model, directory.path()),
            DownloadState::TerminateFailed {
                failure: DeleteFailure::Unload(UnloadFailure::Engine),
            },
            "a stop that could not remove the files is a failed stop"
        );
    }

    /// Stopping a paused download discards its partial file, so the row offers a download again.
    #[gpui_kit::test]
    fn a_terminate_should_discard_the_partial_file(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let path = directory.path().join(model.file_name());
        let part = directory.path().join(format!("{}.part", model.file_name()));
        std::fs::write(&part, b"partial").expect("the partial file is written");

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.set_state_for_test(
                    model,
                    directory.path().to_path_buf(),
                    DownloadState::Paused { downloaded: 7 },
                    cx,
                );
                downloads.terminate(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until(cx, "the stopped download to be discarded", |cx| {
            state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });

        assert!(!part.exists(), "the stopped download's bytes are gone");
        assert!(!path.exists(), "a stopped download leaves no file behind");
    }

    /// Stopping a download in flight cancels its transfer and removes what it wrote.
    #[gpui_kit::test]
    fn a_terminate_should_stop_a_download_in_flight(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(192 * 1024);
        let address = spawn_server(payload, Some(PACE));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let harness = harness(cx);
        let path = directory.path().to_path_buf();
        let part = path.join(format!("{}.part", model.file_name()));

        adopt(cx, &harness, model, &path, address);
        wait_until(cx, "some bytes to arrive before the stop", |cx| {
            matches!(
                state(cx, &harness.downloads, model, &path),
                DownloadState::Downloading { downloaded, .. } if downloaded > 0
            )
        });

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.terminate(model, path.clone(), cx);
            });
        });
        assert_eq!(
            state(cx, &harness.downloads, model, &path),
            DownloadState::Terminating,
            "a download being stopped says so while its files are removed"
        );

        wait_until(cx, "the stopped download to be discarded", |cx| {
            state(cx, &harness.downloads, model, &path) == DownloadState::Idle
        });

        assert!(!part.exists(), "the stopped transfer's bytes are gone");
        assert!(
            !path.join(model.file_name()).exists(),
            "a stopped transfer leaves no file behind"
        );
    }

    /// A failed download is reported on its row.
    #[gpui_kit::test]
    fn a_failed_download_should_be_reported_on_its_row(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let id = DownloadId::new();
        let destination = directory.path().join(model.file_name());

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads
                    .tasks
                    .insert((model, directory.path().to_path_buf()), id);
                downloads.provider_event(
                    &DownloadEvent::TaskUpdated(DownloadTask {
                        id,
                        url: Url::parse("https://example.invalid/model.gguf")
                            .expect("the fixture URL is a URL"),
                        destination,
                        status: DownloadStatus::Failed {
                            failure: DownloadFailure::Cancelled,
                        },
                    }),
                    cx,
                );
            });
        });

        assert_eq!(
            state(cx, &harness.downloads, model, directory.path()),
            DownloadState::DownloadFailed {
                failure: DownloadFailure::Cancelled,
            }
        );
    }

    /// A pause or a resume of a row without a provider task is refused rather than invented.
    #[gpui_kit::test]
    fn pausing_and_resuming_without_a_task_should_be_refused(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;

        let paused = cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.pause(model, directory.path().to_path_buf(), cx)
            })
        });
        let resumed = cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.resume(model, directory.path().to_path_buf(), cx)
            })
        });

        assert!(!paused, "there is no running download to pause");
        assert!(!resumed, "there is no paused download to resume");
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
        let harness = harness_with(
            cx,
            Config::default(),
            Arc::new(move |_: &Path| {
                entered_sender
                    .send(())
                    .expect("the test should be waiting for the unload");
                release_receiver
                    .lock()
                    .expect("the release channel is not poisoned")
                    .recv()
                    .expect("the test should release the unload");
                Ok(())
            }),
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        // The worker only starts once the spawned delete task has been polled, so the executor
        // is turned while the test waits for the unload to be entered.
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
            .advance_clock(DELETE_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();

        let state = state(cx, &harness.downloads, model, directory.path());
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

    /// A directory with no model files offers no downloaded model.
    #[gpui_kit::test]
    fn downloaded_models_should_filter_in_page_order(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");

        assert!(
            cx.update(|cx| harness
                .downloads
                .read(cx)
                .downloaded_models(directory.path()))
                .is_empty()
        );
    }

    #[gpui_kit::test]
    fn a_forced_model_should_be_reported_downloaded_and_in_page_order(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let path = directory.path().to_path_buf();

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.force_downloaded_for_test(TranslationModel::HyMt27BQ8, path.clone(), cx);
                downloads.force_downloaded_for_test(
                    TranslationModel::HyMt218BQ4Km,
                    path.clone(),
                    cx,
                );
            });
        });

        let models = cx.update(|cx| harness.downloads.read(cx).downloaded_models(&path));
        assert_eq!(
            models,
            vec![TranslationModel::HyMt218BQ4Km, TranslationModel::HyMt27BQ8],
            "the list is the page order, not the order the files were marked in"
        );
    }

    #[gpui_kit::test]
    fn state_should_default_to_idle_and_carry_what_was_injected(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let path = TempDir::new()
            .expect("the test directory builds")
            .path()
            .to_path_buf();

        assert_eq!(
            state(cx, &harness.downloads, TranslationModel::HyMt218BQ8, &path),
            DownloadState::Idle
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.set_state_for_test(
                    TranslationModel::HyMt218BQ8,
                    path.clone(),
                    DownloadState::Downloading {
                        downloaded: 7,
                        mb_per_second: 4.0,
                    },
                    cx,
                );
            });
        });
        assert_eq!(
            state(cx, &harness.downloads, TranslationModel::HyMt218BQ8, &path),
            DownloadState::Downloading {
                downloaded: 7,
                mb_per_second: 4.0,
            }
        );
    }

    /// A provider download that finishes is the row's completion: the file is in place, the
    /// directory is recorded and the model is selected.
    #[gpui_kit::test]
    fn a_provider_download_should_complete_and_be_recorded(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(96 * 1024);
        let address = spawn_server(payload.clone(), None);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ8;
        let mut config = Config::default();
        config.translation.model_directory = Some(directory.path().to_path_buf());
        let harness = harness_with(cx, config, Arc::new(|_: &Path| Ok(())));

        adopt(cx, &harness, model, directory.path(), address);
        wait_until(cx, "the download to complete", |cx| {
            state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });

        let path = directory.path().join(model.file_name());
        assert_eq!(
            std::fs::read(&path).expect("the model file is readable"),
            payload.to_vec()
        );
        assert!(
            !directory
                .path()
                .join(format!("{}.part", model.file_name()))
                .exists(),
            "a completed download leaves no partial file"
        );
        assert_eq!(
            cx.update(|cx| cx
                .global::<ConfigManager>()
                .read()
                .translation
                .selected_model),
            Some(model),
            "a finished download becomes the selection when nothing usable was selected"
        );
    }

    /// A paused download keeps its bytes for a resume, and resuming finishes it.
    #[gpui_kit::test]
    fn a_provider_download_should_pause_and_resume(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(192 * 1024);
        let address = spawn_server(payload.clone(), Some(PACE));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let harness = harness(cx);
        let path = directory.path().join(model.file_name());
        let part = directory.path().join(format!("{}.part", model.file_name()));

        adopt(cx, &harness, model, directory.path(), address);
        wait_until(cx, "some bytes to arrive before the pause", |cx| {
            matches!(
                state(cx, &harness.downloads, model, directory.path()),
                DownloadState::Downloading { downloaded, .. } if downloaded > 0
            )
        });

        let paused = cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.pause(model, directory.path().to_path_buf(), cx)
            })
        });
        assert!(paused, "a running download can be paused");
        wait_until(cx, "the download to settle as paused", |cx| {
            matches!(
                state(cx, &harness.downloads, model, directory.path()),
                DownloadState::Paused { downloaded } if downloaded > 0
            )
        });
        assert!(part.exists(), "a paused download keeps its partial file");
        assert!(
            !path.exists(),
            "a paused download is not a destination file"
        );

        let resumed = cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.resume(model, directory.path().to_path_buf(), cx)
            })
        });
        assert!(resumed, "a paused download can be resumed");
        wait_until(cx, "the resumed download to complete", |cx| {
            state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });

        assert_eq!(
            std::fs::read(&path).expect("the model file is readable"),
            payload.to_vec()
        );
        assert!(!part.exists(), "the resumed transfer renames the part");
    }

    /// Asking for a model that already has a paused task continues that task rather than
    /// starting a second download of it.
    #[gpui_kit::test]
    fn a_download_should_resume_the_paused_task_it_already_has(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(160 * 1024);
        let address = spawn_server(payload.clone(), Some(PACE));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ8;
        let harness = harness(cx);
        let path = directory.path().join(model.file_name());
        let part = directory.path().join(format!("{}.part", model.file_name()));

        adopt(cx, &harness, model, directory.path(), address);
        wait_until(cx, "some bytes to arrive before the pause", |cx| {
            matches!(
                state(cx, &harness.downloads, model, directory.path()),
                DownloadState::Downloading { downloaded, .. } if downloaded > 0
            )
        });
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.pause(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until(cx, "the download to settle as paused", |cx| {
            matches!(
                state(cx, &harness.downloads, model, directory.path()),
                DownloadState::Paused { .. }
            )
        });
        assert!(part.exists());

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.download(
                    model,
                    directory.path().to_path_buf(),
                    DownloadSite::HuggingFace,
                    cx,
                );
            });
        });
        wait_until(cx, "the download asked for again to complete", |cx| {
            state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });

        assert_eq!(
            std::fs::read(&path).expect("the model file is readable"),
            payload.to_vec()
        );
        assert!(!part.exists(), "the continued transfer renames the part");
    }

    /// Asking again for a model that already has a paused task continues that task rather than
    /// starting a second download of it, and does not report those bytes under another site.
    #[gpui_kit::test]
    fn a_download_that_resumes_a_task_should_not_change_its_site(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(160 * 1024);
        let address = spawn_server(payload, Some(PACE));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ8;
        let harness = harness(cx);
        let path = directory.path().to_path_buf();

        adopt(cx, &harness, model, &path, address);
        wait_until(cx, "some bytes to arrive before the pause", |cx| {
            matches!(
                state(cx, &harness.downloads, model, &path),
                DownloadState::Downloading { downloaded, .. } if downloaded > 0
            )
        });
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.pause(model, path.clone(), cx);
            });
        });
        wait_until(cx, "the download to settle as paused", |cx| {
            matches!(
                state(cx, &harness.downloads, model, &path),
                DownloadState::Paused { .. }
            )
        });

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.download(model, path.clone(), DownloadSite::ModelScope, cx);
            });
        });

        assert_eq!(
            cx.update(|cx| harness.downloads.read(cx).site(model, &path)),
            None,
            "continuing a task keeps the site it was started from, which was never recorded here"
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, path.clone(), cx);
            });
        });
        wait_until(cx, "the resumed download to be discarded", |cx| {
            state(cx, &harness.downloads, model, &path) == DownloadState::Idle
        });
    }

    /// Deleting a downloaded model unloads the engine and removes the file; the row released its
    /// task when the download completed, so the delete does not wait on one.
    #[gpui_kit::test]
    fn deleting_a_downloaded_model_should_unload_and_remove_its_files(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(64 * 1024);
        let expected = payload.len() as u64;
        let address = spawn_server(payload, None);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ8;
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_for_unload = Arc::clone(&order);
        let harness = harness_with(
            cx,
            Config::default(),
            Arc::new(move |path: &Path| {
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
            }),
        );
        let path = directory.path().join(model.file_name());

        let id = adopt(cx, &harness, model, directory.path(), address);
        wait_until(cx, "the download to complete", |cx| {
            state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });
        assert!(path.exists());
        assert!(
            cx.update(|cx| harness.downloads.read(cx).tasks.is_empty()),
            "a completed download releases the row's task"
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until_deleted(cx, &harness.downloads, model, directory.path(), &path);

        assert_eq!(
            *order.lock().expect("the order log is not poisoned"),
            ["unload-before-remove"],
            "the engine must release the file before it is removed"
        );
        assert_eq!(
            cx.update(|cx| harness.provider.read(cx).task(id).map(|task| task.status)),
            Some(DownloadStatus::Completed {
                downloaded: expected,
                total: expected,
            }),
            "the provider keeps its completed record; the delete does not need it"
        );
    }

    /// A finished download leaves no task behind: the provider still holds its completed record,
    /// but the row no longer refers to it.
    #[gpui_kit::test]
    fn a_completed_download_should_release_its_task(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(64 * 1024);
        let expected = payload.len() as u64;
        let address = spawn_server(payload, None);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ8;
        let harness = harness(cx);

        let id = adopt(cx, &harness, model, directory.path(), address);
        wait_until(cx, "the download to complete", |cx| {
            state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });

        assert!(
            cx.update(|cx| harness.downloads.read(cx).tasks.is_empty()),
            "the completed download is no longer the row's task"
        );
        assert_eq!(
            cx.update(|cx| harness.provider.read(cx).task(id).map(|task| task.status)),
            Some(DownloadStatus::Completed {
                downloaded: expected,
                total: expected,
            }),
            "the provider completed the task the row released"
        );
    }

    /// A file removed outside the application is downloaded again: with no task left the row adds
    /// a new one instead of finding the completed download it already finished.
    #[gpui_kit::test]
    fn a_model_deleted_outside_the_application_should_download_again(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(64 * 1024);
        let address = spawn_server(payload, None);
        let local_url =
            Url::parse(&format!("http://{address}/model")).expect("the test URL is a URL");
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ8;
        let harness = harness(cx);
        let path = directory.path().join(model.file_name());
        let key = (model, directory.path().to_path_buf());

        let completed = adopt(cx, &harness, model, directory.path(), address);
        wait_until(cx, "the download to complete", |cx| {
            state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });
        std::fs::remove_file(&path)
            .expect("the downloaded file is removed outside the application");
        assert!(!path.exists());
        assert!(
            cx.update(|cx| harness.provider.read(cx).task(completed).is_some()),
            "the provider still holds the completed task, so the new one does not exist by chance"
        );
        assert!(
            cx.update(|cx| harness.downloads.read(cx).tasks.is_empty()),
            "the row released the completed task when its download finished"
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.start_download(model, directory.path().to_path_buf(), local_url, cx);
            });
        });

        let rediscovered = cx.update(|cx| harness.downloads.read(cx).tasks.get(&key).copied());
        let rediscovered =
            rediscovered.expect("a file removed outside the application is downloaded again");
        assert_ne!(
            rediscovered, completed,
            "the download that starts is a new task, not the completed one"
        );

        // The new task transfers from the test server, not from the model's published address;
        // it is removed before its outcome is waited for, so the test never depends on it.
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until(cx, "the new download to be removed", |cx| {
            cx.update(|cx| harness.downloads.read(cx).tasks.is_empty())
        });
        assert_eq!(
            state(cx, &harness.downloads, model, directory.path()),
            DownloadState::Idle
        );
    }

    /// A failed download can be discarded like a paused one: the manager removes its provider
    /// task and partial file, and the row settles back to offering the download again.
    #[gpui_kit::test]
    fn deleting_a_failed_download_should_remove_its_part_and_reset_the_row(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let payload = payload(64 * 1024);
        let address = spawn_server(payload, None);
        let local_url =
            Url::parse(&format!("http://{address}/model")).expect("the test URL is a URL");
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ8;
        let harness = harness(cx);
        let path = directory.path().join(model.file_name());
        let part = directory.path().join(format!("{}.part", model.file_name()));

        // The test server answers fewer bytes than the model publishes, so the task settles as
        // failed with the bytes that did arrive still in its partial file.
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.start_download(model, directory.path().to_path_buf(), local_url, cx);
            });
        });
        wait_until(cx, "the download to fail", |cx| {
            matches!(
                state(cx, &harness.downloads, model, directory.path()),
                DownloadState::DownloadFailed { .. }
            )
        });
        assert!(
            part.exists(),
            "a failed transfer keeps the bytes it did arrive as its partial file"
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until(cx, "the failed download to be deleted", |cx| {
            !part.exists()
                && state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });

        assert!(!part.exists(), "the partial file is removed");
        assert!(!path.exists(), "nothing was renamed into place");
        assert!(
            cx.update(|cx| harness.downloads.read(cx).tasks.is_empty()),
            "the discarded download keeps no provider task"
        );
    }

    /// Deleting a paused download removes its partial file through the manager.
    #[gpui_kit::test]
    fn deleting_a_paused_download_should_remove_the_part(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(192 * 1024);
        let address = spawn_server(payload, Some(PACE));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let harness = harness(cx);
        let path = directory.path().join(model.file_name());
        let part = directory.path().join(format!("{}.part", model.file_name()));

        adopt(cx, &harness, model, directory.path(), address);
        wait_until(cx, "some bytes to arrive before the pause", |cx| {
            matches!(
                state(cx, &harness.downloads, model, directory.path()),
                DownloadState::Downloading { downloaded, .. } if downloaded > 0
            )
        });
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.pause(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until(cx, "the download to settle as paused", |cx| {
            matches!(
                state(cx, &harness.downloads, model, directory.path()),
                DownloadState::Paused { .. }
            )
        });
        assert!(part.exists());

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, directory.path().to_path_buf(), cx);
            });
        });
        wait_until(cx, "the paused download to be deleted", |cx| {
            !part.exists()
                && state(cx, &harness.downloads, model, directory.path()) == DownloadState::Idle
        });

        assert!(!part.exists(), "the partial file is removed");
        assert!(!path.exists(), "nothing was renamed into place");
        assert!(
            cx.update(|cx| harness.downloads.read(cx).tasks.is_empty()),
            "the deleted model keeps no provider task"
        );
    }

    /// A download records the site it was started from, which is what tells a retry whether it
    /// can continue the task or has to start again.
    #[gpui_kit::test]
    fn a_download_should_record_the_site_it_was_started_from(cx: &mut TestAppContext) {
        let harness = harness(cx);
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ4Km;
        let path = directory.path().to_path_buf();

        // The seam turns the transfers of this manager into records, so no request leaves the
        // process; it needs a row of its own to hold the state it simulates.
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.set_transfers_deferred_for_test(
                    TranslationModel::HyMt218BQ4Km,
                    path.clone(),
                    DownloadSite::HuggingFace,
                    cx,
                );
                downloads.download(model, path.clone(), DownloadSite::ModelScope, cx);
            });
        });

        assert_eq!(
            cx.update(|cx| harness.downloads.read(cx).site(model, &path)),
            Some(DownloadSite::ModelScope),
            "the site the download was asked for is the site the row recorded"
        );
        assert_eq!(
            state(cx, &harness.downloads, model, &path),
            DownloadState::Downloading {
                downloaded: 0,
                mb_per_second: 0.0,
            }
        );
    }

    /// A site is unknown until a download records one; a retry with no recorded site continues
    /// the failed task from the same site rather than deleting it.
    #[gpui_kit::test]
    fn a_same_site_retry_should_continue_the_failed_task(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(64 * 1024);
        let address = spawn_server(payload, None);
        let local_url =
            Url::parse(&format!("http://{address}/model")).expect("the test URL is a URL");
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ8;
        let harness = harness(cx);
        let path = directory.path().to_path_buf();

        assert_eq!(
            cx.update(|cx| harness.downloads.read(cx).site(model, &path)),
            None,
            "a download that never ran records no site"
        );

        // The test server answers fewer bytes than the model publishes, so the task fails with
        // the bytes that did arrive still in its partial file.
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.start_download(model, path.clone(), local_url, cx);
            });
        });
        wait_until(cx, "the download to fail", |cx| {
            matches!(
                state(cx, &harness.downloads, model, &path),
                DownloadState::DownloadFailed { .. }
            )
        });
        let failed_task = cx.update(|cx| {
            harness
                .downloads
                .read(cx)
                .tasks
                .get(&(model, path.clone()))
                .copied()
                .expect("the failed download keeps its provider task")
        });

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.retry(model, path.clone(), DownloadSite::HuggingFace, cx);
            });
        });

        assert_eq!(
            state(cx, &harness.downloads, model, &path),
            DownloadState::Downloading {
                downloaded: fs::metadata(
                    directory.path().join(format!("{}.part", model.file_name()))
                )
                .map(|part| part.len())
                .unwrap_or(0),
                mb_per_second: 0.0,
            },
            "a retry at the same site continues the failed task from its partial file"
        );
        assert_eq!(
            cx.update(|cx| harness
                .downloads
                .read(cx)
                .tasks
                .get(&(model, path.clone()))
                .copied()),
            Some(failed_task),
            "the retry resumes the task the download already had"
        );
        assert_eq!(
            cx.update(|cx| harness.downloads.read(cx).site(model, &path)),
            Some(DownloadSite::HuggingFace),
            "the retry records the site it resumed from"
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.delete(model, path.clone(), cx);
            });
        });
        wait_until(cx, "the retried download to be discarded", |cx| {
            state(cx, &harness.downloads, model, &path) == DownloadState::Idle
        });
    }

    /// A retry at another site deletes what the previous site transferred and starts again from
    /// zero, after the delete has finished rather than while it is still removing the files.
    #[gpui_kit::test]
    fn a_different_site_retry_should_delete_then_restart(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let payload = payload(192 * 1024);
        let address = spawn_server(payload, Some(PACE));
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let harness = harness(cx);
        let path = directory.path().to_path_buf();
        let part = path.join(format!("{}.part", model.file_name()));

        adopt(cx, &harness, model, &path, address);
        wait_until(cx, "some bytes to arrive before the pause", |cx| {
            matches!(
                state(cx, &harness.downloads, model, &path),
                DownloadState::Downloading { downloaded, .. } if downloaded > 0
            )
        });
        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.pause(model, path.clone(), cx);
            });
        });
        wait_until(cx, "the download to settle as paused", |cx| {
            matches!(
                state(cx, &harness.downloads, model, &path),
                DownloadState::Paused { .. }
            )
        });
        assert!(part.exists());

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.set_transfers_deferred_for_test(
                    TranslationModel::HyMt27BQ8,
                    path.clone(),
                    DownloadSite::HuggingFace,
                    cx,
                );
                downloads.retry(model, path.clone(), DownloadSite::ModelScope, cx);
            });
        });
        assert_eq!(
            state(cx, &harness.downloads, model, &path),
            DownloadState::Deleting,
            "another site discards the previous site's files before it downloads"
        );

        wait_until(cx, "the restart from the other site", |cx| {
            state(cx, &harness.downloads, model, &path)
                == DownloadState::Downloading {
                    downloaded: 0,
                    mb_per_second: 0.0,
                }
                && cx.update(|cx| harness.downloads.read(cx).site(model, &path))
                    == Some(DownloadSite::ModelScope)
        });

        assert!(
            !part.exists(),
            "the partial file of the previous site is gone before the new download starts"
        );
        assert!(
            cx.update(|cx| harness.downloads.read(cx).tasks.is_empty()),
            "the previous site's task is gone before the new download starts"
        );
    }

    /// A restart whose delete fails reports the delete and keeps no intent to restart later.
    #[gpui_kit::test]
    fn a_restart_that_cannot_delete_should_report_the_failure(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt218BQ4Km;
        let path = directory.path().to_path_buf();
        std::fs::write(path.join(model.file_name()), b"model")
            .expect("the fixture file is written");
        let harness = harness_with(
            cx,
            Config::default(),
            Arc::new(|_: &Path| Err(UnloadFailure::TimedOut)),
        );

        cx.update(|cx| {
            harness.downloads.update(cx, |downloads, cx| {
                downloads.set_state_for_test(
                    model,
                    path.clone(),
                    DownloadState::DownloadFailed {
                        failure: DownloadFailure::Transport,
                    },
                    cx,
                );
                downloads.retry(model, path.clone(), DownloadSite::ModelScope, cx);
            });
        });

        wait_until(cx, "the restart's delete to fail", |cx| {
            matches!(
                state(cx, &harness.downloads, model, &path),
                DownloadState::DeleteFailed { .. }
            )
        });

        assert!(
            cx.update(|cx| harness.downloads.read(cx).pending_restarts.is_empty()),
            "a delete that failed must not keep the retry for later"
        );
        assert_eq!(
            cx.update(|cx| harness.downloads.read(cx).site(model, &path)),
            None,
            "no site was recorded for a restart that never started"
        );
        assert_eq!(
            state(cx, &harness.downloads, model, &path),
            DownloadState::DeleteFailed {
                failure: DeleteFailure::Unload(UnloadFailure::TimedOut),
            },
            "a delete that failed is a failure the row reports, not a delete it completed"
        );
    }
}

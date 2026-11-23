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

const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DELETE_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnloadFailure {
    TimedOut,
    Engine,
}

#[derive(Clone)]
pub struct DownloadService {
    pub tasks: Signal<Vec<DownloadTask>>,
    pub add: Rc<dyn Fn(DownloadRequest, DownloadId)>,
    pub pause: Rc<dyn Fn(DownloadId)>,
    pub resume: Rc<dyn Fn(DownloadId)>,
    pub delete: Rc<dyn Fn(DownloadId)>,
}

pub struct TranslationDownloadsDeps {
    pub store: SettingsStore,
    pub downloads: DownloadService,
    pub runtime: tokio::runtime::Handle,
    pub unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum DownloadState {
    #[default]
    Idle,
    Downloading {
        downloaded: u64,
        mb_per_second: f32,
    },
    Pausing {
        downloaded: u64,
    },
    Paused {
        downloaded: u64,
    },
    DownloadFailed {
        failure: DownloadFailure,
    },
    Deleting,
    Terminating,
    DeleteFailed {
        failure: DeleteFailure,
    },
    TerminateFailed {
        failure: DeleteFailure,
    },
}

impl DownloadState {
    fn removal(&self) -> Option<Removal> {
        match self {
            Self::Deleting => Some(Removal::Delete),
            Self::Terminating => Some(Removal::Terminate),
            _ => None,
        }
    }

    pub(crate) fn is_removing(&self) -> bool {
        self.removal().is_some()
    }

    fn is_failed_removal(&self) -> bool {
        matches!(
            self,
            Self::DeleteFailed { .. } | Self::TerminateFailed { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    Delete,
    Terminate,
}

impl Removal {
    fn running(self) -> DownloadState {
        match self {
            Self::Delete => DownloadState::Deleting,
            Self::Terminate => DownloadState::Terminating,
        }
    }

    fn failed(self, failure: DeleteFailure) -> DownloadState {
        match self {
            Self::Delete => DownloadState::DeleteFailed { failure },
            Self::Terminate => DownloadState::TerminateFailed { failure },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeleteFailure {
    Unload(UnloadFailure),
    Io { kind: io::ErrorKind },
    TimedOut,
    Provider(DownloadFailure),
}

type RowKey = (TranslationModel, PathBuf);

#[derive(Clone, Debug)]
struct TaskEntry {
    id: DownloadId,
    seen: bool,
    last: Option<DownloadStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingKind {
    UnloadTask(DownloadId),
    Files,
}

struct PendingOutcome {
    key: RowKey,
    removal: Removal,
    kind: PendingKind,
    result: Result<(), DeleteFailure>,
}

pub struct TranslationDownloads {
    store: SettingsStore,
    downloads: DownloadService,
    runtime: tokio::runtime::Handle,
    unload: Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>,
    wake: AppHandle,
    delete_timeout: Cell<Duration>,
    states: RefCell<AHashMap<RowKey, DownloadState>>,
    tasks: RefCell<AHashMap<RowKey, TaskEntry>>,
    sites: RefCell<AHashMap<RowKey, DownloadSite>>,
    pending_restarts: RefCell<AHashMap<RowKey, DownloadSite>>,
    removals: RefCell<AHashMap<RowKey, Removal>>,
    completions: Arc<Mutex<Vec<PendingOutcome>>>,
    #[cfg(test)]
    forced_downloaded: RefCell<ahash::AHashSet<RowKey>>,
}

impl TranslationDownloads {
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

    pub(crate) fn row_state(&self, model: TranslationModel, directory: &Path) -> DownloadState {
        self.states
            .borrow()
            .get(&(model, directory.to_path_buf()))
            .cloned()
            .unwrap_or_default()
    }

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

    pub(crate) fn downloaded_models(&self, directory: &Path) -> Vec<TranslationModel> {
        TranslationModel::ALL
            .into_iter()
            .filter(|model| self.is_downloaded(*model, directory))
            .collect()
    }

    pub(crate) fn is_available(&self, model: TranslationModel, directory: &Path) -> bool {
        !self.row_state(model, directory).is_removing() && self.is_downloaded(model, directory)
    }

    pub(crate) fn available_models(&self, directory: &Path) -> Vec<TranslationModel> {
        self.downloaded_models(directory)
            .into_iter()
            .filter(|model| self.is_available(*model, directory))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn site(&self, model: TranslationModel, directory: &Path) -> Option<DownloadSite> {
        self.sites
            .borrow()
            .get(&(model, directory.to_path_buf()))
            .copied()
    }

    pub(crate) fn download(&self, model: TranslationModel, directory: PathBuf, site: DownloadSite) {
        let key = (model, directory.clone());
        let recorded_site = self.sites.borrow().get(&key).copied();
        let task = self.tasks.borrow().get(&key).cloned();
        if task.is_some() && recorded_site.is_some_and(|recorded| recorded != site) {
            return;
        }
        let restart_pending = self.pending_restarts.borrow().contains_key(&key);
        if restart_pending
            || self.is_downloaded(model, &directory)
            || self.row_state(model, &directory).is_removing()
        {
            return;
        }

        if let Some(entry) = task {
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

    pub(crate) fn retry(
        &self,
        app: &mut App,
        model: TranslationModel,
        directory: PathBuf,
        site: DownloadSite,
    ) {
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

    pub(crate) fn pause(&self, model: TranslationModel, directory: PathBuf) -> bool {
        let Some(entry) = self.tasks.borrow().get(&(model, directory)).cloned() else {
            return false;
        };
        (self.downloads.pause)(entry.id);
        true
    }

    pub(crate) fn resume(&self, model: TranslationModel, directory: PathBuf) -> bool {
        let Some(entry) = self.tasks.borrow().get(&(model, directory)).cloned() else {
            return false;
        };
        (self.downloads.resume)(entry.id);
        true
    }

    pub(crate) fn delete(&self, app: &mut App, model: TranslationModel, directory: PathBuf) {
        self.remove(app, model, directory, Removal::Delete);
    }

    pub(crate) fn terminate(&self, app: &mut App, model: TranslationModel, directory: PathBuf) {
        self.remove(app, model, directory, Removal::Terminate);
    }

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
        let files_exist = path.exists()
            || download_provider::partial::path(&path).is_some_and(|part| part.exists());
        let task = self.tasks.borrow().get(&key).map(|entry| entry.id);
        self.sites.borrow_mut().remove(&key);
        self.removals.borrow_mut().insert(key.clone(), removal);
        self.states
            .borrow_mut()
            .insert(key.clone(), removal.running());

        match task {
            Some(id) if path.exists() => {
                self.spawn_unload(key, removal, PendingKind::UnloadTask(id), path);
            }
            Some(id) => (self.downloads.delete)(id),
            None if files_exist => {
                self.spawn_unload(key, removal, PendingKind::Files, path);
            }
            None => {
                self.finish_removal(app, key, removal, Ok(()));
            }
        }
    }

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

    fn apply_outcome(&self, app: &mut App, outcome: PendingOutcome) -> bool {
        match outcome.kind {
            PendingKind::Files => {
                self.finish_removal(app, outcome.key, outcome.removal, outcome.result)
            }
            PendingKind::UnloadTask(id) => match outcome.result {
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

    fn finish_removal(
        &self,
        app: &mut App,
        key: RowKey,
        removal: Removal,
        result: Result<(), DeleteFailure>,
    ) -> bool {
        let outcome = match result {
            Ok(()) => Ok(()),
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

                let restart = self.pending_restarts.borrow_mut().remove(&key);
                if let Some(site) = restart {
                    self.download(model, directory, site);
                }
                true
            }
            Err(failure) => {
                self.removals.borrow_mut().remove(&key);
                self.pending_restarts.borrow_mut().remove(&key);
                self.states
                    .borrow_mut()
                    .insert(key, removal.failed(failure));
                true
            }
        }
    }

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

    fn apply_status(
        &self,
        app: &mut App,
        key: &RowKey,
        previous: Option<&DownloadStatus>,
        status: &DownloadStatus,
    ) -> bool {
        let removal = self.removals.borrow().get(key).copied();
        if let Some(removal) = removal {
            return match status {
                DownloadStatus::Deleting => self.set_state(key.clone(), removal.running()),
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
            DownloadStatus::Deleting => current
                .removal()
                .map_or(DownloadState::Deleting, Removal::running),
            DownloadStatus::Completed { .. } => {
                return self.complete_download(app, key);
            }
            DownloadStatus::Failed { failure } => match current.removal() {
                Some(removal) => removal.failed(delete_failure(*failure)),
                None if current.is_failed_removal() => return false,
                None => DownloadState::DownloadFailed { failure: *failure },
            },
        };
        self.set_state(key.clone(), next)
    }

    fn task_vanished(&self, app: &mut App, key: &RowKey) -> bool {
        match self.row_state(key.0, &key.1).removal() {
            Some(removal) => self.finish_removal(app, key.clone(), removal, Ok(())),
            None => {
                self.forget(key);
                true
            }
        }
    }

    fn take_outcomes(&self) -> Vec<PendingOutcome> {
        std::mem::take(
            &mut *self
                .completions
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn forget(&self, key: &RowKey) {
        self.states.borrow_mut().remove(key);
        self.tasks.borrow_mut().remove(key);
        self.sites.borrow_mut().remove(key);
    }

    fn set_state(&self, key: RowKey, state: DownloadState) -> bool {
        let mut states = self.states.borrow_mut();
        if states.get(&key) == Some(&state) {
            return false;
        }
        states.insert(key, state);
        true
    }

    fn mark_seen(&self, key: &RowKey) {
        if let Some(entry) = self.tasks.borrow_mut().get_mut(key) {
            entry.seen = true;
        }
    }

    fn observe_status(&self, key: &RowKey, status: &DownloadStatus) -> Option<DownloadStatus> {
        let mut tasks = self.tasks.borrow_mut();
        let entry = tasks.get_mut(key)?;
        let previous = entry.last.clone();
        entry.last = Some(status.clone());
        previous
    }

    fn write_change(&self, app: &mut App, change: SettingsChange) {
        let window = first_window(app);
        let Some(window) = window else {
            log::warn!("the translation downloads found no window to write the selection through");
            return;
        };
        let _ = self.store.write(app, window, change);
    }

    #[cfg(test)]
    pub(crate) fn force_downloaded_for_test(&self, model: TranslationModel, directory: PathBuf) {
        self.forced_downloaded
            .borrow_mut()
            .insert((model, directory));
    }

    #[cfg(test)]
    pub(crate) fn forget_downloaded_for_test(&self, model: TranslationModel, directory: PathBuf) {
        self.forced_downloaded
            .borrow_mut()
            .remove(&(model, directory));
    }

    #[cfg(test)]
    pub(crate) fn set_state_for_test(
        &self,
        model: TranslationModel,
        directory: PathBuf,
        state: DownloadState,
    ) {
        self.states.borrow_mut().insert((model, directory), state);
    }

    #[cfg(test)]
    pub(crate) fn set_delete_timeout_for_test(&self, timeout: Duration) {
        self.delete_timeout.set(timeout);
    }
}

pub(crate) fn file_size_matches(path: &Path, expected: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected)
}

fn first_window(app: &App) -> Option<WindowId> {
    app.windows().next().map(|(window, _)| window)
}

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

fn delete_failure(failure: DownloadFailure) -> DeleteFailure {
    match failure {
        DownloadFailure::Io {
            kind: io::ErrorKind::TimedOut,
        } => DeleteFailure::TimedOut,
        DownloadFailure::Io { kind } => DeleteFailure::Io { kind },
        failure => DeleteFailure::Provider(failure),
    }
}

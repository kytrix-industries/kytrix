//! The tests of the translation model downloads.
//!
//! The download manager's own tests: what a row shows for each task snapshot the application's
//! download manager publishes, what a delete unloads and removes, and which command a pause, a
//! resume or a retry issues. The manager is driven with a fake download manager
//! ([`FakeDownloads`]) and the application's setting harness, so no test needs a network, a real
//! transfer or a sleeping executor.

use std::{
    cell::RefCell,
    io,
    path::Path,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use app_config::{
    settings::Config,
    translation::{DownloadSite, TranslationModel},
};
use download_provider::{
    error::DownloadFailure,
    task::{DownloadId, DownloadStatus},
};
use tempfile::TempDir;
use ui_core::{node::signal::Signal, test_support::app::TestApp, window::WindowId};

use crate::{
    session::{SettingsChange, SettingsStore},
    settings_tests::test_support::{FakeDownloads, Unload, download_task},
    translation_downloads::{
        DeleteFailure, DownloadState, TranslationDownloads, TranslationDownloadsDeps,
        UnloadFailure, file_size_matches,
    },
};

/// How long a test waits for an asynchronous removal before failing.
const WAIT_LIMIT: Duration = Duration::from_secs(10);

/// The model most download tests use.
fn model() -> TranslationModel {
    TranslationModel::HyMt218BQ4Km
}

/// The settings, download manager and translation downloads one test drives.
struct Downloads {
    app: TestApp,
    window: WindowId,
    store: SettingsStore,
    config: Signal<Config>,
    fake: FakeDownloads,
    manager: Rc<TranslationDownloads>,
    /// The configuration snapshots the store published, in write order.
    writes: Rc<RefCell<Vec<Config>>>,
}

impl Downloads {
    /// The manager over the default settings and an unload that succeeds.
    fn new() -> Self {
        Self::with(Config::default(), Arc::new(|_: &Path| Ok(())))
    }

    /// The manager over `config` and `unload`.
    fn with(config: Config, unload: Unload) -> Self {
        let mut app = TestApp::new();
        let window = app.window_id();
        let config_signal = app.app_mut().signal(config);
        let writes = Rc::new(RefCell::new(Vec::new()));
        let fail_next = Rc::new(RefCell::new(None));
        let store = {
            let writes = Rc::clone(&writes);
            let fail_next = Rc::clone(&fail_next);
            SettingsStore::new(app.app_mut(), config_signal, move |target, candidate| {
                if let Some(failure) = fail_next.borrow_mut().take() {
                    return Err(failure);
                }
                target.write(&config_signal, candidate.clone());
                writes.borrow_mut().push(candidate);
                Ok(())
            })
        };
        let fake = FakeDownloads::new(app.app_mut());
        let runtime = app.app().runtime_handle().clone();
        let manager = TranslationDownloads::new(
            app.app_mut(),
            TranslationDownloadsDeps {
                store: store.clone(),
                downloads: fake.service(),
                runtime,
                unload,
            },
        );

        Self {
            app,
            window,
            store,
            config: config_signal,
            fake,
            manager,
            writes,
        }
    }

    /// The settings as they are stored now.
    fn config(&self) -> Config {
        self.app.app().read_with(&self.config, Config::clone)
    }

    /// Writes one change through the session.
    fn write(&mut self, change: SettingsChange) {
        let window = self.window;
        self.store
            .write(self.app.app_mut(), window, change)
            .expect("the settings accept the change");
    }

    /// The snapshots the session stored.
    fn writes(&self) -> Vec<Config> {
        self.writes.borrow().clone()
    }

    /// Mirrors the download manager's snapshots onto the rows.
    fn sync(&mut self) -> bool {
        let manager = Rc::clone(&self.manager);
        manager.sync(self.app.app_mut())
    }

    /// Publishes one task snapshot under `model` in `directory`.
    fn publish(
        &mut self,
        model: TranslationModel,
        directory: &Path,
        id: DownloadId,
        status: DownloadStatus,
    ) {
        self.publish_id(model, directory, id, status);
    }

    /// Publishes one task snapshot and answers its identity.
    fn publish_id(
        &mut self,
        model: TranslationModel,
        directory: &Path,
        id: DownloadId,
        status: DownloadStatus,
    ) -> DownloadId {
        let task = download_task(id, directory.join(model.file_name()), status);
        self.fake.push(self.app.app_mut(), task);
        id
    }

    /// The identity the manager asked its download manager to add last.
    fn added(&self) -> DownloadId {
        self.fake
            .added()
            .expect("a download was added to the fake manager")
    }

    /// The row state of `model` in `directory`.
    fn state(&self, model: TranslationModel, directory: &Path) -> DownloadState {
        self.manager.row_state(model, directory)
    }

    /// Runs frames until `done` holds, failing after [`WAIT_LIMIT`].
    fn spin(&mut self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT_LIMIT;
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.sync();
            std::thread::yield_now();
        }
    }
}

/// A callback that signals when a blocking unload started, and the channel it signals on.
fn entered() -> (Arc<dyn Fn() + Send + Sync>, mpsc::Receiver<()>) {
    let (sender, receiver) = mpsc::channel();
    let sender = Mutex::new(sender);
    let entered = Arc::new(move || {
        sender
            .lock()
            .expect("the entered channel is never poisoned")
            .send(())
            .ok();
    });
    (entered, receiver)
}

/// An unload that succeeds, after recording that it ran while the file still existed.
fn order_unload(order: Arc<Mutex<Vec<&'static str>>>) -> Unload {
    Arc::new(move |path: &Path| {
        let seen = if path.exists() {
            "unload-before-remove"
        } else {
            "unload-after-remove"
        };
        order
            .lock()
            .expect("the order log is never poisoned")
            .push(seen);
        Ok(())
    })
}

/// The configuration over `directory`, so a finished download records its selection there.
fn config_in(directory: &Path) -> Config {
    let mut config = Config::default();
    config.translation.model_directory = Some(directory.to_path_buf());
    config
}

/// Deleting a file that is already gone is what the row asked for, not a failure.
#[test]
fn deleting_a_missing_file_should_succeed() {
    let directory = TempDir::new().expect("the test directory builds");
    let mut downloads = Downloads::new();
    let path = directory.path().to_path_buf();

    downloads
        .manager
        .delete(downloads.app.app_mut(), model(), path.clone());
    downloads.sync();

    assert_eq!(
        downloads.state(model(), &path),
        DownloadState::Idle,
        "a delete that found nothing to remove must not be left failed"
    );
}

/// The engine is unloaded while the file still exists; only then is the file removed.
#[test]
fn a_delete_should_unload_before_it_removes_the_file() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"model").expect("the fixture file is written");
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut downloads = Downloads::with(Config::default(), order_unload(Arc::clone(&order)));

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.spin("the delete to finish", |downloads| {
        !path.exists() && downloads.state(model, directory.path()) == DownloadState::Idle
    });

    assert_eq!(
        *order.lock().expect("the order log is never poisoned"),
        ["unload-before-remove"],
        "the engine must release the file before the delete removes it"
    );
}

/// A file the process downloaded is unloaded before the download manager is asked to remove the
/// task that owns it.
#[test]
fn a_delete_should_unload_before_the_manager_removes_a_task() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"model").expect("the fixture file is written");
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut downloads = Downloads::with(Config::default(), order_unload(Arc::clone(&order)));
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Downloading {
            downloaded: 1,
            total: Some(2),
            mb_per_second: 0.0,
        },
    );
    downloads.sync();

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.spin("the manager to be asked to delete", |downloads| {
        downloads.fake.deleted(id)
    });

    assert_eq!(
        *order.lock().expect("the order log is never poisoned"),
        ["unload-before-remove"],
        "the engine releases the file before the manager removes the task"
    );
    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Deleting,
        "the row stays in its removal until the task is gone"
    );

    downloads.fake.remove(downloads.app.app_mut(), id);
    downloads.sync();
    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Idle
    );
}

/// A selection that was made while the removal was already running is cleared when the removal
/// finishes: the files it names are gone by then.
#[test]
fn a_finished_removal_should_clear_a_selection_made_while_it_ran() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"model").expect("the fixture file is written");
    let (mark, entered_rx) = entered();
    let (release_sender, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let mut downloads = Downloads::with(
        config_in(directory.path()),
        Arc::new(move |_: &Path| {
            mark();
            release_rx
                .lock()
                .expect("the release channel is never poisoned")
                .recv()
                .expect("the test releases the unload");
            Ok(())
        }),
    );
    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    entered_rx
        .recv_timeout(WAIT_LIMIT)
        .expect("the delete should reach its unload");

    // What a stale menu choice or another writer may have stored after the removal started: the
    // selection was not there when the removal began to look for it.
    downloads.write(SettingsChange::TranslationModelSelected { model: Some(model) });
    release_sender
        .send(())
        .expect("the delete worker is waiting");
    downloads.spin("the removal to finish", |downloads| {
        downloads.state(model, directory.path()) == DownloadState::Idle
    });

    assert_eq!(
        downloads.config().translation.selected_model,
        None,
        "a selection that still names the removed model is cleared when the removal finishes"
    );
}

/// A finished removal only clears the selection when it names the directory the removal emptied:
/// the same model in another directory is still there.
#[test]
fn a_finished_removal_should_keep_a_selection_in_another_directory() {
    let removed = TempDir::new().expect("the removed directory builds");
    let chosen = TempDir::new().expect("the chosen directory builds");
    let model = model();
    let mut config = config_in(chosen.path());
    config.translation.selected_model = Some(model);
    let mut downloads = Downloads::with(config, Arc::new(|_: &Path| Ok(())));

    downloads
        .manager
        .delete(downloads.app.app_mut(), model, removed.path().to_path_buf());
    downloads.sync();

    assert_eq!(
        downloads.config().translation.selected_model,
        Some(model),
        "the selection names a directory the removal did not empty, so it is kept"
    );
}

/// A failed unload leaves the row reporting the reason instead of hanging in Deleting.
#[test]
fn a_failed_unload_should_be_reported_as_a_failed_delete() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"model").expect("the fixture file is written");
    let mut downloads = Downloads::with(
        Config::default(),
        Arc::new(|_: &Path| Err(UnloadFailure::TimedOut)),
    );

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.spin("the failed unload to be reported", |downloads| {
        matches!(
            downloads.state(model, directory.path()),
            DownloadState::DeleteFailed {
                failure: DeleteFailure::Unload(UnloadFailure::TimedOut),
            }
        )
    });
}

/// A terminate that fails to unload reports it as the failed stop it is, not as a delete.
#[test]
fn a_terminate_should_report_its_own_failure() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    std::fs::write(directory.path().join(model.file_name()), b"model")
        .expect("the fixture file is written");
    let mut downloads = Downloads::with(
        Config::default(),
        Arc::new(|_: &Path| Err(UnloadFailure::Engine)),
    );

    downloads.manager.terminate(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.spin("the failed stop to be reported", |downloads| {
        matches!(
            downloads.state(model, directory.path()),
            DownloadState::TerminateFailed {
                failure: DeleteFailure::Unload(UnloadFailure::Engine),
            }
        )
    });
}

/// Stopping a paused download discards its partial file, so the row offers a download again.
#[test]
fn a_terminate_should_discard_the_partial_file() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().join(model.file_name());
    let part = directory.path().join(format!("{}.part", model.file_name()));
    std::fs::write(&part, b"partial").expect("the partial file is written");
    let mut downloads = Downloads::new();
    downloads.manager.set_state_for_test(
        model,
        directory.path().to_path_buf(),
        DownloadState::Paused { downloaded: 7 },
    );

    downloads.manager.terminate(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.spin("the stopped download to be discarded", |downloads| {
        downloads.state(model, directory.path()) == DownloadState::Idle
    });

    assert!(!part.exists(), "the stopped download's bytes are gone");
    assert!(!path.exists(), "a stopped download leaves no file behind");
}

/// Stopping a download in flight asks the manager to remove the task that is transferring it.
#[test]
fn a_terminate_should_remove_the_task_of_a_download_in_flight() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Downloading {
            downloaded: 7,
            total: Some(100),
            mb_per_second: 1.5,
        },
    );
    downloads.sync();

    downloads.manager.terminate(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Terminating,
        "a download being stopped says so while its files are removed"
    );
    assert!(
        downloads.fake.deleted(id),
        "stopping a running download removes its task"
    );

    downloads.fake.remove(downloads.app.app_mut(), id);
    downloads.sync();
    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Idle
    );
}

/// A failed download is reported on its row.
#[test]
fn a_failed_download_should_be_reported_on_its_row() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Failed {
            failure: DownloadFailure::Cancelled,
        },
    );
    downloads.sync();

    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::DownloadFailed {
            failure: DownloadFailure::Cancelled,
        }
    );
}

/// A pause or a resume of a row without a manager task is refused rather than invented.
#[test]
fn pausing_and_resuming_without_a_task_should_be_refused() {
    let directory = TempDir::new().expect("the test directory builds");
    let downloads = Downloads::new();
    let path = directory.path().to_path_buf();

    assert!(
        !downloads.manager.pause(model(), path.clone()),
        "there is no running download to pause"
    );
    assert!(
        !downloads.manager.resume(model(), path),
        "there is no paused download to resume"
    );
}

/// A delete worker that never finishes is reported as failed instead of staying Deleting.
#[test]
fn a_delete_that_never_finishes_should_be_reported_as_failed() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    std::fs::write(directory.path().join(model.file_name()), b"model")
        .expect("the fixture file is written");
    let (mark, entered_rx) = entered();
    let (release_sender, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let mut downloads = Downloads::with(
        Config::default(),
        Arc::new(move |_: &Path| {
            mark();
            release_rx
                .lock()
                .expect("the release channel is never poisoned")
                .recv()
                .expect("the test releases the unload");
            Ok(())
        }),
    );
    downloads
        .manager
        .set_delete_timeout_for_test(Duration::ZERO);

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    entered_rx
        .recv_timeout(WAIT_LIMIT)
        .expect("the delete should reach its unload");
    downloads.spin("the delete to time out", |downloads| {
        matches!(
            downloads.state(model, directory.path()),
            DownloadState::DeleteFailed {
                failure: DeleteFailure::TimedOut,
            }
        )
    });
    release_sender
        .send(())
        .expect("the delete worker is waiting");
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

/// A forced model is reported downloaded and the list is in page order; a directory with no model
/// files offers none.
#[test]
fn downloaded_models_should_be_reported_in_page_order() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().to_path_buf();
    let downloads = Downloads::new();

    assert!(downloads.manager.downloaded_models(&path).is_empty());
    downloads
        .manager
        .force_downloaded_for_test(TranslationModel::HyMt27BQ8, path.clone());
    downloads
        .manager
        .force_downloaded_for_test(model, path.clone());

    assert_eq!(
        downloads.manager.downloaded_models(&path),
        vec![model, TranslationModel::HyMt27BQ8],
        "the list is the page order, not the order the files were marked in"
    );
}

/// A row state defaults to idle and carries what was injected.
#[test]
fn state_should_default_to_idle_and_carry_what_was_injected() {
    let directory = TempDir::new().expect("the test directory builds");
    let path = directory.path().to_path_buf();
    let downloads = Downloads::new();

    assert_eq!(
        downloads.state(TranslationModel::HyMt218BQ8, &path),
        DownloadState::Idle
    );
    downloads.manager.set_state_for_test(
        TranslationModel::HyMt218BQ8,
        path.clone(),
        DownloadState::Downloading {
            downloaded: 7,
            mb_per_second: 4.0,
        },
    );
    assert_eq!(
        downloads.state(TranslationModel::HyMt218BQ8, &path),
        DownloadState::Downloading {
            downloaded: 7,
            mb_per_second: 4.0,
        }
    );
}

/// A download that finishes is the row's completion: the task is released and the model is
/// selected when nothing usable was selected.
#[test]
fn a_completed_download_should_be_recorded_and_release_its_task() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::with(config_in(directory.path()), Arc::new(|_: &Path| Ok(())));
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads
        .manager
        .force_downloaded_for_test(model, directory.path().to_path_buf());
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Completed {
            downloaded: 100,
            total: 100,
        },
    );
    downloads.sync();

    assert_eq!(
        downloads.config().translation.selected_model,
        Some(model),
        "a finished download becomes the selection when nothing usable was selected"
    );
    assert!(
        !downloads
            .manager
            .pause(model, directory.path().to_path_buf()),
        "a completed download releases the row's task"
    );
    assert!(
        downloads.config().translation.model_directory == Some(directory.path().to_path_buf()),
        "the directory the download arrived in is the stored one"
    );
}

/// A finished download does not replace a model the directory already loads and has selected.
#[test]
fn a_completed_download_should_keep_a_usable_selection() {
    let directory = TempDir::new().expect("the test directory builds");
    let selected = TranslationModel::HyMt218BQ4Km;
    let finished = TranslationModel::HyMt27BQ8;
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(selected);
    let mut downloads = Downloads::with(config, Arc::new(|_: &Path| Ok(())));
    downloads
        .manager
        .force_downloaded_for_test(selected, directory.path().to_path_buf());
    downloads.manager.download(
        finished,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads
        .manager
        .force_downloaded_for_test(finished, directory.path().to_path_buf());
    downloads.publish(
        finished,
        directory.path(),
        id,
        DownloadStatus::Completed {
            downloaded: 100,
            total: 100,
        },
    );
    downloads.sync();

    assert_eq!(
        downloads.config().translation.selected_model,
        Some(selected),
        "a usable selection is not replaced by another finished download"
    );
}

/// A pause and a resume are commands to the download manager.
#[test]
fn a_paused_download_should_be_paused_and_resumed_through_the_manager() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Downloading {
            downloaded: 7,
            total: Some(100),
            mb_per_second: 1.0,
        },
    );
    downloads.sync();

    assert!(
        downloads
            .manager
            .pause(model, directory.path().to_path_buf())
    );
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Paused {
            downloaded: 7,
            total: Some(100),
        },
    );
    downloads.sync();
    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Paused { downloaded: 7 }
    );
    assert!(
        downloads.fake.paused(id),
        "pausing asks the manager to pause the task"
    );

    assert!(
        downloads
            .manager
            .resume(model, directory.path().to_path_buf())
    );
    assert!(
        downloads.fake.resumed(id),
        "resuming asks the manager to continue the task"
    );
}

/// Asking for a model that already has a paused task continues that task rather than starting a
/// second download of it.
#[test]
fn a_download_should_resume_the_paused_task_it_already_has() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Paused {
            downloaded: 7,
            total: Some(100),
        },
    );
    downloads.sync();

    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );

    assert_eq!(
        downloads.fake.adds().len(),
        1,
        "the second request continues the task instead of adding another"
    );
    assert!(downloads.fake.resumed(id), "the paused task is continued");
}

/// A download started from one site is left alone when another site is asked for: continuing it
/// would report the bytes of one site under another.
#[test]
fn a_download_should_not_change_the_site_of_the_task_it_has() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Paused {
            downloaded: 7,
            total: Some(100),
        },
    );
    downloads.sync();

    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::ModelScope,
    );

    assert_eq!(
        downloads.manager.site(model, directory.path()),
        Some(DownloadSite::HuggingFace),
        "the task keeps the site it was started from"
    );
    assert_eq!(
        downloads.fake.adds().len(),
        1,
        "no second transfer is started for the other site"
    );
}

/// A download records the site it was started from, and the request is for that site's address.
#[test]
fn a_download_should_record_the_site_it_was_started_from() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let downloads = Downloads::new();

    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::ModelScope,
    );

    assert_eq!(
        downloads.manager.site(model, directory.path()),
        Some(DownloadSite::ModelScope)
    );
    match downloads.fake.adds().first() {
        Some((request, _)) => {
            assert_eq!(
                request.url.as_str(),
                model.download_url(DownloadSite::ModelScope)
            );
            assert_eq!(
                request.destination,
                directory.path().join(model.file_name())
            );
            assert_eq!(request.expected_size, Some(model.size_bytes()));
        }
        other => panic!("the download manager should be asked to add a task: {other:?}"),
    }
}

/// A model the directory already holds at its published size is not downloaded again.
#[test]
fn a_download_of_a_file_that_is_in_place_should_be_refused() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads
        .manager
        .force_downloaded_for_test(model, directory.path().to_path_buf());
    downloads.sync();

    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );

    assert!(
        downloads.fake.adds().is_empty(),
        "a model whose file is in place is not transferred again"
    );
}

/// A paused download is discarded when its row is deleted: the manager removes its task and the
/// row goes back to offering a download.
#[test]
fn deleting_a_paused_download_should_remove_its_task() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let part = directory.path().join(format!("{}.part", model.file_name()));
    std::fs::write(&part, b"partial").expect("the partial file is written");
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Paused {
            downloaded: 7,
            total: Some(100),
        },
    );
    downloads.sync();

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    assert!(
        downloads.fake.deleted(id),
        "the paused download's task is removed"
    );

    downloads.fake.remove(downloads.app.app_mut(), id);
    downloads.sync();
    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Idle
    );
}

/// A failed download can be discarded like a paused one: the manager removes its task and partial
/// file, and the row settles back to offering the download again.
#[test]
fn deleting_a_failed_download_should_reset_the_row() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let part = directory.path().join(format!("{}.part", model.file_name()));
    std::fs::write(&part, b"partial").expect("the partial file is written");
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Failed {
            failure: DownloadFailure::Transport,
        },
    );
    downloads.sync();

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.fake.remove(downloads.app.app_mut(), id);
    downloads.sync();

    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Idle
    );
    assert!(
        downloads.fake.deleted(id),
        "the failed download's task is removed"
    );
}

/// A download that completed releases its task; a file removed outside the application is
/// downloaded again under a new task.
#[test]
fn a_model_deleted_outside_the_application_should_download_again() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"model").expect("the fixture file is written");
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let first = downloads.added();
    downloads
        .manager
        .force_downloaded_for_test(model, directory.path().to_path_buf());
    downloads.publish(
        model,
        directory.path(),
        first,
        DownloadStatus::Completed {
            downloaded: 4,
            total: 4,
        },
    );
    downloads.sync();
    assert!(
        !downloads
            .manager
            .pause(model, directory.path().to_path_buf())
    );

    std::fs::remove_file(&path).expect("the downloaded file is removed outside the application");
    downloads
        .manager
        .forget_downloaded_for_test(model, directory.path().to_path_buf());
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );

    let second = downloads.added();
    assert_ne!(
        first, second,
        "the download that starts is a new task, not the completed one"
    );
}

/// A delete of a downloaded model unloads the engine and removes the file; the row released its
/// task when the download completed, so the delete does not wait on one.
#[test]
fn deleting_a_downloaded_model_should_unload_and_remove_its_files() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"model").expect("the fixture file is written");
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut downloads = Downloads::with(Config::default(), order_unload(Arc::clone(&order)));
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads
        .manager
        .force_downloaded_for_test(model, directory.path().to_path_buf());
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Completed {
            downloaded: 4,
            total: 4,
        },
    );
    downloads.sync();
    assert!(path.exists());

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.spin("the downloaded model to be removed", |downloads| {
        !path.exists() && downloads.state(model, directory.path()) == DownloadState::Idle
    });

    assert_eq!(
        *order.lock().expect("the order log is never poisoned"),
        ["unload-before-remove"],
        "the engine must release the file before it is removed"
    );
}

/// A retry at the site the failed download came from continues that task.
#[test]
fn a_same_site_retry_should_continue_the_failed_task() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::ModelScope,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Failed {
            failure: DownloadFailure::Transport,
        },
    );
    downloads.sync();

    downloads.manager.retry(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
        DownloadSite::ModelScope,
    );

    assert!(
        downloads.fake.resumed(id),
        "a retry at the same site continues the failed task from its partial file"
    );
    assert_eq!(
        downloads.manager.site(model, directory.path()),
        Some(DownloadSite::ModelScope)
    );
}

/// A retry at another site deletes what the previous site transferred and starts again from it
/// once the removal has finished.
#[test]
fn a_different_site_retry_should_delete_then_restart() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let part = directory.path().join(format!("{}.part", model.file_name()));
    std::fs::write(&part, b"partial").expect("the partial file is written");
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Failed {
            failure: DownloadFailure::Transport,
        },
    );
    downloads.sync();

    downloads.manager.retry(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
        DownloadSite::ModelScope,
    );
    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::Deleting,
        "another site discards the previous site's files before it downloads"
    );
    assert!(
        downloads.fake.deleted(id),
        "the previous site's task is removed first"
    );

    downloads.fake.remove(downloads.app.app_mut(), id);
    downloads.sync();
    let adds = downloads.fake.adds();
    assert_eq!(
        adds.len(),
        2,
        "the restart adds the transfer from the chosen site"
    );
    match adds.last() {
        Some((request, _)) => assert_eq!(
            request.url.as_str(),
            model.download_url(DownloadSite::ModelScope)
        ),
        None => panic!("the restart should add a task"),
    }
    assert_eq!(
        downloads.manager.site(model, directory.path()),
        Some(DownloadSite::ModelScope),
        "the restart records the site the user chose"
    );
}

/// A restart whose delete fails reports the delete and keeps no intent to restart later.
#[test]
fn a_restart_that_cannot_delete_should_report_the_failure() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    std::fs::write(directory.path().join(model.file_name()), b"model")
        .expect("the fixture file is written");
    let mut downloads = Downloads::with(
        Config::default(),
        Arc::new(|_: &Path| Err(UnloadFailure::TimedOut)),
    );
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Failed {
            failure: DownloadFailure::Transport,
        },
    );
    downloads.sync();

    downloads.manager.retry(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
        DownloadSite::ModelScope,
    );
    downloads.spin("the restart's delete to fail", |downloads| {
        matches!(
            downloads.state(model, directory.path()),
            DownloadState::DeleteFailed { .. }
        )
    });

    assert_eq!(
        downloads.state(model, directory.path()),
        DownloadState::DeleteFailed {
            failure: DeleteFailure::Unload(UnloadFailure::TimedOut),
        }
    );
    assert_eq!(
        downloads.manager.site(model, directory.path()),
        None,
        "no site was recorded for a restart that never started"
    );
    assert_eq!(
        downloads.fake.adds().len(),
        1,
        "a delete that failed must not keep the retry for later"
    );
}

/// Two deletes of the same row issue one removal.
#[test]
fn deleting_a_row_twice_should_issue_one_removal() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Downloading {
            downloaded: 7,
            total: Some(100),
            mb_per_second: 1.0,
        },
    );
    downloads.sync();

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );

    assert_eq!(
        downloads.fake.delete_count(),
        1,
        "a removal that is already running is not asked for again"
    );
}

/// A delete of a row the settings do not select writes nothing.
#[test]
fn a_delete_of_an_unselected_model_should_write_nothing() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::with(config_in(directory.path()), Arc::new(|_: &Path| Ok(())));

    downloads.manager.delete(
        downloads.app.app_mut(),
        model,
        directory.path().to_path_buf(),
    );
    downloads.sync();

    assert!(
        downloads.writes().is_empty(),
        "a delete that found nothing to remove writes nothing"
    );
}

/// Why a download failure is remembered: the row's state is what the page draws, and the failure
/// survives frames without being overwritten.
#[test]
fn a_failed_download_should_stay_reported_across_frames() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = model();
    let mut downloads = Downloads::new();
    downloads.manager.download(
        model,
        directory.path().to_path_buf(),
        DownloadSite::HuggingFace,
    );
    let id = downloads.added();
    downloads.publish(
        model,
        directory.path(),
        id,
        DownloadStatus::Failed {
            failure: DownloadFailure::Io {
                kind: io::ErrorKind::PermissionDenied,
            },
        },
    );
    downloads.sync();
    let first = downloads.state(model, directory.path());
    assert!(!downloads.sync(), "a steady frame moves nothing");
    assert_eq!(downloads.state(model, directory.path()), first);
}

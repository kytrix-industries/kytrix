//! The application's download manager and the two seams its owners ask through.
//!
//! One manager belongs to the process ([`download_provider::provider::DownloadProvider`]):
//! the settings page's translation models and the updater's release payload both download
//! through it, so a pause in one place is a pause of the same transfer, and there is one
//! network route for both. This module is the composition root's half:
//!
//! - [`Downloads::new`] builds the manager over the application transport, runtime and
//!   network projection;
//! - [`Downloads::settings_service`] adapts it to the settings page's [`DownloadService`]
//!   seam (one signal of task snapshots plus the four commands);
//! - [`Downloads::updater_service`] adapts it to the updater's [`UpdaterDownloads`] seam
//!   (the command trait plus one event subscription);
//! - [`Downloads::sync`] mirrors the manager's snapshots onto the signal the settings page
//!   reads, once per frame, and only when something moved.
//!
//! The manager publishes events from the threads its transfers run on. The updater's sink
//! pushes into the updater's own inbox and wakes the engine; the settings page has no
//! event stream by design and reads the signal this module keeps in step, which means a
//! settings write never has to cross a thread.

use std::{path::Path, rc::Rc, sync::Arc};

use app_config::network::Config;
use app_config_provider::notification::ConfigNotification;
use download_provider::{
    provider::{DownloadProvider, DownloadProviderDeps},
    request::DownloadRequest,
    task::{DownloadId, DownloadTask},
};
use network::transport::hub::TransportHub;
use settings_ui::translation_downloads::{DownloadService, UnloadFailure};
use ui_core::{app::App, node::signal::Signal};
use updater_ui::download::{
    DownloadService as UpdaterDownloads, DownloadSubscription as UpdaterDownloadSubscription,
};
use widget_tokio_runtime::runtime::Runtime;

/// The process-wide download manager and the settings page's view of it.
pub(crate) struct Downloads {
    provider: Rc<DownloadProvider>,
    /// Every task snapshot the manager last published, as the settings page reads them.
    tasks: Signal<Vec<DownloadTask>>,
}

impl Downloads {
    /// Builds the manager over the application's transport, runtime and network settings.
    pub(crate) fn new(
        app: &mut App,
        transport: Arc<TransportHub>,
        runtime: Runtime,
        network: ConfigNotification<Config>,
    ) -> Self {
        Self {
            provider: Rc::new(DownloadProvider::new(
                DownloadProviderDeps { transport, runtime },
                network,
            )),
            tasks: app.signal(Vec::new()),
        }
    }

    /// Mirrors the manager's task snapshots onto the signal the settings page reads.
    ///
    /// A steady frame writes nothing (I3); a transfer's progress publishes a new snapshot
    /// on the manager once an interval and this is the frame that puts it in front of the
    /// page.
    pub(crate) fn sync(&self, app: &mut App) {
        let tasks = self.provider.tasks();
        if app.read_with(&self.tasks, |current| current != &tasks) {
            app.write(&self.tasks, tasks);
        }
    }

    /// The settings page's seam over the manager.
    pub(crate) fn settings_service(&self) -> DownloadService {
        let add = {
            let provider = Rc::clone(&self.provider);
            move |request: DownloadRequest, id: DownloadId| provider.add(id, request)
        };
        let pause = {
            let provider = Rc::clone(&self.provider);
            move |id: DownloadId| {
                provider.pause(id);
            }
        };
        let resume = {
            let provider = Rc::clone(&self.provider);
            move |id: DownloadId| {
                provider.resume(id);
            }
        };
        let delete = {
            let provider = Rc::clone(&self.provider);
            move |id: DownloadId| {
                provider.delete(id);
            }
        };

        DownloadService {
            tasks: self.tasks,
            add: Rc::new(add),
            pause: Rc::new(pause),
            resume: Rc::new(resume),
            delete: Rc::new(delete),
        }
    }

    /// The updater's seam over the manager.
    pub(crate) fn updater_service(self: &Rc<Self>) -> Rc<dyn UpdaterDownloads> {
        Rc::new(AppDownloads {
            provider: Rc::clone(&self.provider),
        })
    }

    /// The manager, for the shell's tests.
    #[cfg(test)]
    pub(crate) fn provider(&self) -> &Rc<DownloadProvider> {
        &self.provider
    }

    /// The task snapshots the settings page reads, for the shell's tests.
    #[cfg(test)]
    pub(crate) const fn tasks_signal(&self) -> Signal<Vec<DownloadTask>> {
        self.tasks
    }
}

/// The updater's view of the process-wide manager.
struct AppDownloads {
    provider: Rc<DownloadProvider>,
}

impl UpdaterDownloads for AppDownloads {
    fn add(&self, request: DownloadRequest) -> DownloadId {
        let id = DownloadId::new();
        self.provider.add(id, request);
        id
    }

    fn pause(&self, id: DownloadId) -> bool {
        self.provider.pause(id)
    }

    fn resume(&self, id: DownloadId) -> bool {
        self.provider.resume(id)
    }

    fn delete(&self, id: DownloadId) -> bool {
        self.provider.delete(id)
    }

    fn task(&self, id: DownloadId) -> Option<DownloadTask> {
        self.provider.task(id)
    }

    fn subscribe(
        &self,
        sink: Arc<dyn Fn(download_provider::task::DownloadEvent) + Send + Sync>,
    ) -> UpdaterDownloadSubscription {
        let subscription = self.provider.subscribe(sink);
        UpdaterDownloadSubscription::new(move || drop(subscription))
    }
}

/// The translation engine's unload, as the settings page removes a model file.
///
/// The engine may hold the model open; a removal asks it to release the file first, and a
/// hung unload is bounded by the page's own deadline ([`UnloadFailure::TimedOut`]).
#[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
pub(crate) fn translation_unload() -> Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>
{
    Arc::new(|path| {
        ai_translator::translator::unload_model(path).map_err(|error| match error {
            ai_translator::error::TranslationError::UnloadTimeout { .. } => UnloadFailure::TimedOut,
            _ => UnloadFailure::Engine,
        })
    })
}

/// Windows ARM64 has no llama.cpp build, so there is no engine that could hold a file.
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
pub(crate) fn translation_unload() -> Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>
{
    Arc::new(|_: &Path| Ok(()))
}

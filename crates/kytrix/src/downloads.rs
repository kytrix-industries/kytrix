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
use tokio::runtime::Handle;
use ui_core::{app::App, node::signal::Signal};
use updater_ui::download::{
    DownloadService as UpdaterDownloads, DownloadSubscription as UpdaterDownloadSubscription,
};

pub(crate) struct Downloads {
    provider: Rc<DownloadProvider>,
    tasks: Signal<Vec<DownloadTask>>,
}

impl Downloads {
    pub(crate) fn new(
        app: &mut App,
        transport: Arc<TransportHub>,
        runtime: Handle,
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

    pub(crate) fn sync(&self, app: &mut App) {
        let tasks = self.provider.tasks();
        if app.read_with(&self.tasks, |current| current != &tasks) {
            app.write(&self.tasks, tasks);
        }
    }

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

    pub(crate) fn updater_service(self: &Rc<Self>) -> Rc<dyn UpdaterDownloads> {
        Rc::new(AppDownloads {
            provider: Rc::clone(&self.provider),
        })
    }

    #[cfg(test)]
    pub(crate) fn provider(&self) -> &Rc<DownloadProvider> {
        &self.provider
    }

    #[cfg(test)]
    pub(crate) const fn tasks_signal(&self) -> Signal<Vec<DownloadTask>> {
        self.tasks
    }
}

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

#[cfg(ai_translation)]
pub(crate) fn translation_unload() -> Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>
{
    Arc::new(|path| {
        ai_translator::translator::unload_model(path).map_err(|error| match error {
            ai_translator::error::TranslationError::UnloadTimeout { .. } => UnloadFailure::TimedOut,
            _ => UnloadFailure::Engine,
        })
    })
}

#[cfg(not(ai_translation))]
pub(crate) fn translation_unload() -> Arc<dyn Fn(&Path) -> Result<(), UnloadFailure> + Send + Sync>
{
    Arc::new(|_: &Path| Ok(()))
}

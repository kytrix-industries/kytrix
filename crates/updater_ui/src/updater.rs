use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
    time::Duration,
};

use ahash::AHashMap;
use app_config::network::{
    Config as NetworkConfig, KytrixServiceCapability, RouteTarget, ServiceTarget,
};
use bytes::Bytes;
use download_provider::{
    error::DownloadFailure,
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadStatus, DownloadTask},
};
use i18n::{t_smol, translate::Translate};
use network::transport::{http::HttpClientClass, hub::TransportHub};
use smol_str::{SmolStr, format_smolstr};
use tokio::runtime::Handle;
use ui_core::{
    app::{App, AppHandle, TimerId},
    error::EngineError,
    node::signal::{Signal, SignalScope},
    window::WindowId,
};
use url::Url;
use widget::{frame::RouteId, services::WidgetServices};

use crate::{
    dialog::{DialogData, UpdateDialog},
    download::{DownloadService, DownloadSubscription},
    error::{InstallError, MetadataError, UpdaterError},
    fetch, install, publish,
    release::ReleaseInfo,
    state::{CANCEL_RETRY_DELAY, FIRST_CHECK_DELAY, UpdaterState, retry_delay},
};

const DEFAULT_KEY: &str = "kytrix.updater";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadPhase {
    Downloading,
    Pausing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdaterView {
    Notes,
    Download {
        downloaded: u64,
        total: Option<u64>,
        phase: DownloadPhase,
    },
    Downloaded,
    Installing,
    Failure(&'static str),
}

impl UpdaterView {
    fn of(state: &UpdaterState) -> Option<Self> {
        match state {
            UpdaterState::Idle | UpdaterState::Checking => None,
            UpdaterState::Available(_) => Some(Self::Notes),
            UpdaterState::Downloading(download) => Some(Self::Download {
                downloaded: download.accepted(),
                total: download.named_total(),
                phase: DownloadPhase::Downloading,
            }),
            UpdaterState::Pausing(download) => Some(Self::Download {
                downloaded: download.accepted(),
                total: download.named_total(),
                phase: DownloadPhase::Pausing,
            }),
            UpdaterState::Paused(download) => Some(Self::Download {
                downloaded: download.accepted(),
                total: download.named_total(),
                phase: DownloadPhase::Paused,
            }),
            UpdaterState::ReadyToInstall(_) => Some(Self::Downloaded),
            UpdaterState::Installing(_) => Some(Self::Installing),
            UpdaterState::Failed(failed) => Some(Self::Failure(failed.error.message_key())),
        }
    }

    #[must_use]
    pub fn offered(self) -> bool {
        matches!(self, Self::Notes | Self::Failure(_))
    }

    #[must_use]
    pub fn phase(self) -> Option<DownloadPhase> {
        match self {
            Self::Download { phase, .. } => Some(phase),
            _ => None,
        }
    }

    #[must_use]
    pub fn percent(self) -> Option<f32> {
        let Self::Download {
            downloaded, total, ..
        } = self
        else {
            return None;
        };
        total
            .filter(|total| *total > 0)
            .map(|total| (downloaded as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32)
    }
}

pub struct UpdaterDeps {
    pub transport: Arc<TransportHub>,
    pub downloads: Rc<dyn DownloadService>,
    pub endpoint: Option<Url>,
}

impl UpdaterDeps {
    #[must_use]
    pub fn new(transport: Arc<TransportHub>, downloads: Rc<dyn DownloadService>) -> Self {
        Self {
            transport,
            downloads,
            endpoint: None,
        }
    }

    #[must_use]
    pub fn with_endpoint(mut self, endpoint: Url) -> Self {
        self.endpoint = Some(endpoint);
        self
    }
}

enum Message {
    Checked {
        generation: u64,
        result: Result<Option<ReleaseInfo>, UpdaterError>,
    },
    PayloadRead {
        generation: u64,
        task: DownloadId,
        result: Result<Vec<u8>, UpdaterError>,
    },
    Installed {
        generation: u64,
        result: Result<(), InstallError>,
    },
    Download {
        generation: u64,
        event: DownloadEvent,
    },
}

type Inbox = Arc<Mutex<VecDeque<Message>>>;

fn push_message(inbox: &Inbox, message: Message) {
    if let Ok(mut queue) = inbox.lock() {
        queue.push_back(message);
    }
}

fn drain_messages(inbox: &Inbox) -> Vec<Message> {
    match inbox.lock() {
        Ok(mut queue) => queue.drain(..).collect(),
        Err(poisoned) => poisoned.into_inner().drain(..).collect(),
    }
}

#[derive(Clone)]
pub struct Updater {
    core: Rc<RefCell<UpdaterCore>>,
}

impl Updater {
    pub fn install(
        app: &mut App,
        window: WindowId,
        services: Rc<WidgetServices>,
        deps: UpdaterDeps,
    ) -> Self {
        Self::install_with_key(app, window, services, deps, DEFAULT_KEY)
    }

    pub fn install_with_key(
        app: &mut App,
        window: WindowId,
        services: Rc<WidgetServices>,
        deps: UpdaterDeps,
        key: impl Into<SmolStr>,
    ) -> Self {
        let key = key.into();
        let translate = services.translate.clone();
        let runtime = services.runtime.clone();
        let scope = app.signal_scope();
        let view = app.signal_scoped(scope, None);
        let inbox: Inbox = Arc::new(Mutex::new(VecDeque::new()));
        let wake = app.handle();
        let generation = 1;
        let sink = {
            let inbox = Arc::clone(&inbox);
            let wake = wake.clone();
            Arc::new(move |event: DownloadEvent| {
                push_message(&inbox, Message::Download { generation, event });
                wake.wake();
            })
        };
        let subscription = deps.downloads.subscribe(sink);
        let core = Rc::new(RefCell::new(UpdaterCore {
            window,
            services: Rc::clone(&services),
            translate,
            key,
            scope,
            view,
            transport: deps.transport,
            downloads: deps.downloads,
            endpoint: deps.endpoint,
            runtime,
            wake,
            inbox,
            generation,
            state: UpdaterState::Idle,
            last_view: None,
            check_timer: None,
            check_running: false,
            installing: false,
            reading: None,
            download_task: None,
            cleanups: AHashMap::default(),
            failed_checks: 0,
            subscription: Some(subscription),
            dialog: None,
            dialog_requested: false,
            route: None,
            released: false,
            self_weak: Weak::new(),
        }));

        {
            let mut core_ref = core.borrow_mut();
            core_ref.self_weak = Rc::downgrade(&core);
            let weak = Rc::downgrade(&core);
            core_ref.route = Some(services.frame_router.register(move |app| {
                let Some(core) = weak.upgrade() else {
                    return Ok(());
                };
                core.borrow_mut().sync(app)
            }));
            core_ref.start(app);
        }

        Self { core }
    }

    #[must_use]
    pub fn view(&self, app: &App) -> Option<UpdaterView> {
        let core = self.core.borrow();
        *app.read(&core.view)
    }

    #[must_use]
    pub(crate) fn view_signal(&self) -> Signal<Option<UpdaterView>> {
        self.core.borrow().view
    }

    pub fn open_dialog(&self, app: &mut App) {
        let mut core = self.core.borrow_mut();
        if core.released {
            return;
        }
        core.dialog_requested = true;
        if let Err(error) = core.sync_dialog(app) {
            log::warn!("opening the update dialog failed: {error}");
        }
    }

    #[must_use]
    pub fn is_released(&self) -> bool {
        self.core.borrow().released
    }

    pub fn release(&self, app: &mut App) -> usize {
        let mut core = self.core.borrow_mut();
        if core.released {
            return 0;
        }
        core.released = true;
        core.generation = core.generation.saturating_add(1);
        if let Some(route) = core.route.take() {
            core.services.frame_router.unregister(route);
        }
        if let Some(timer) = core.check_timer.take() {
            app.cancel_timer(&timer);
        }
        let _ = core.subscription.take();
        if let Some(dialog) = core.dialog.as_mut() {
            dialog.release(app);
        }
        for task in core.cleanups.keys().copied().collect::<Vec<_>>() {
            core.downloads.delete(task);
        }
        core.cleanups.clear();
        app.release_scope(core.scope)
    }
}

impl std::fmt::Debug for Updater {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let core = self.core.borrow();
        formatter
            .debug_struct("Updater")
            .field("state", &core.state)
            .field("view", &core.last_view)
            .field("released", &core.released)
            .finish_non_exhaustive()
    }
}

pub(crate) struct UpdaterCore {
    window: WindowId,
    services: Rc<WidgetServices>,
    translate: Translate,
    key: SmolStr,
    scope: SignalScope,
    view: Signal<Option<UpdaterView>>,
    transport: Arc<TransportHub>,
    downloads: Rc<dyn DownloadService>,
    endpoint: Option<Url>,
    runtime: Handle,
    wake: AppHandle,
    inbox: Inbox,
    generation: u64,
    state: UpdaterState,
    last_view: Option<UpdaterView>,
    check_timer: Option<TimerId>,
    check_running: bool,
    installing: bool,
    reading: Option<DownloadId>,
    download_task: Option<DownloadId>,
    cleanups: AHashMap<DownloadId, u8>,
    failed_checks: usize,
    subscription: Option<DownloadSubscription>,
    dialog: Option<UpdateDialog>,
    dialog_requested: bool,
    route: Option<RouteId>,
    released: bool,
    self_weak: Weak<RefCell<UpdaterCore>>,
}

impl UpdaterCore {
    pub(crate) fn services(&self) -> &WidgetServices {
        &self.services
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }
        for message in drain_messages(&self.inbox) {
            self.apply(app, message);
        }
        self.publish_view(app);
        self.sync_dialog(app)
    }

    fn apply(&mut self, app: &mut App, message: Message) {
        match message {
            Message::Checked { generation, result } if generation == self.generation => {
                self.finish_check(app, result);
            }
            Message::PayloadRead {
                generation,
                task,
                result,
            } if generation == self.generation => {
                self.finish_payload(app, task, result);
            }
            Message::Installed { generation, result } if generation == self.generation => {
                self.finish_install(app, result);
            }
            Message::Download { generation, event } if generation == self.generation => {
                self.download_event(app, &event);
            }
            _ => {}
        }
    }

    fn publish_view(&mut self, app: &mut App) {
        let view = UpdaterView::of(&self.state);
        if view == self.last_view {
            return;
        }
        self.last_view = view;
        let _ = app.write(&self.view, view);
    }

    fn start(&mut self, app: &mut App) {
        self.schedule_check(app, FIRST_CHECK_DELAY);
    }

    fn schedule_check(&mut self, app: &mut App, delay: Duration) {
        let weak = self.self_weak.clone();
        self.check_timer = app
            .after(self.window, delay, move |app| {
                if let Some(core) = weak.upgrade() {
                    core.borrow_mut().check_now(app);
                }
            })
            .ok();
    }

    fn check_now(&mut self, app: &mut App) {
        if self.released || self.check_running || !self.state.begin_check() {
            return;
        }
        self.check_timer = None;
        self.check_running = true;
        self.publish_view(app);

        let endpoint = match self.endpoint.clone() {
            Some(endpoint) => endpoint,
            None => match Url::parse(publish::METADATA_ENDPOINT) {
                Ok(endpoint) => endpoint,
                Err(source) => {
                    self.finish_check(
                        app,
                        Err(UpdaterError::Metadata(MetadataError::Endpoint(source))),
                    );
                    return;
                }
            },
        };
        let transport = Arc::clone(&self.transport);
        let config: NetworkConfig =
            app.read_with(&self.services.config, |config| config.network_config());
        let resolving = self.runtime.clone();
        let wake = self.wake.clone();
        let inbox = Arc::clone(&self.inbox);
        let generation = self.generation;
        let cancellation = self.transport.cancellation().child_token();

        self.runtime.spawn(async move {
            let result =
                fetch::fetch_release(&transport, &config, &resolving, &endpoint, &cancellation)
                    .await;
            push_message(&inbox, Message::Checked { generation, result });
            wake.wake();
        });
    }

    fn finish_check(&mut self, app: &mut App, result: Result<Option<ReleaseInfo>, UpdaterError>) {
        self.check_running = false;
        self.check_timer = None;
        match result {
            Ok(release) => {
                self.failed_checks = 0;
                self.state.check_finished(release);
            }
            Err(error) => {
                log::debug!("the update check failed: {error}");
                self.state = UpdaterState::Idle;
                let retry = retry_delay(self.failed_checks);
                self.failed_checks += 1;
                if let Some(delay) = retry {
                    self.schedule_check(app, delay);
                }
            }
        }
        self.publish_view(app);
    }

    fn download(&mut self, app: &mut App) {
        if !install::supported() {
            return;
        }
        let Some(release) = self.state.offered().cloned() else {
            return;
        };
        if let Some(task) = self.download_task {
            match self.downloads.task(task).map(|task| task.status) {
                Some(DownloadStatus::Paused { .. } | DownloadStatus::Failed { .. }) => {
                    if self.downloads.resume(task) && self.state.begin_download(task).is_some() {
                        self.publish_view(app);
                    }
                    return;
                }
                Some(DownloadStatus::Downloading { .. } | DownloadStatus::Pausing { .. }) => {
                    return;
                }
                Some(DownloadStatus::Completed { .. }) => {
                    return;
                }
                Some(DownloadStatus::Deleting) | None => self.download_task = None,
            }
        }
        let destination = std::env::temp_dir()
            .join(format_smolstr!("kytrix-update-{}.bin", DownloadId::new()).as_str());
        let request = DownloadRequest {
            url: release.asset.url,
            destination,
            target: RouteTarget::Service(ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::UpdaterDownload,
            }),
            class: HttpClientClass::UpdaterDownload,
            expected_size: None,
        };
        let task = self.downloads.add(request);
        if self.state.begin_download(task).is_none() {
            log::debug!("the offered update disappeared while its download was being added");
            self.downloads.delete(task);
            self.download_task = None;
            return;
        }
        self.download_task = Some(task);
        self.publish_view(app);
    }

    fn download_event(&mut self, app: &mut App, event: &DownloadEvent) {
        match event {
            DownloadEvent::TaskRemoved(id) if self.cleanups.remove(id).is_some() => {
                return;
            }
            DownloadEvent::TaskUpdated(task) if self.cleanups.contains_key(&task.id) => {
                self.cleanup_failed(app, task);
                return;
            }
            _ => {}
        }
        let DownloadEvent::TaskUpdated(task) = event else {
            return;
        };
        let Some(download) = self.state.download() else {
            return;
        };
        if download.task != task.id {
            return;
        }
        match &task.status {
            DownloadStatus::Downloading {
                downloaded, total, ..
            } => {
                if self.state.record_progress(task.id, *downloaded, *total) {
                    self.publish_view(app);
                }
            }
            DownloadStatus::Pausing { .. } => {
                if self.state.pause_started() {
                    self.publish_view(app);
                }
            }
            DownloadStatus::Paused { downloaded, total } => {
                if self.state.pause_finished(*downloaded, *total) {
                    self.publish_view(app);
                }
            }
            DownloadStatus::Completed { .. } => {
                self.read_payload(task.id, task.destination.clone());
            }
            DownloadStatus::Failed { failure } => {
                self.state.attempt_failed(UpdaterError::Download(*failure));
                self.publish_view(app);
            }
            DownloadStatus::Deleting => {}
        }
    }

    fn cleanup_failed(&mut self, app: &mut App, task: &DownloadTask) {
        let DownloadStatus::Failed { failure } = task.status else {
            return;
        };
        let Some(retries) = self.cleanups.get_mut(&task.id) else {
            return;
        };
        if *retries > 0 {
            log::warn!(
                "the update download to {:?} could not be deleted: {failure:?}",
                task.destination,
            );
            self.cleanups.remove(&task.id);
            return;
        }
        log::warn!(
            "the update download to {:?} could not be deleted: {failure:?}; asking again",
            task.destination,
        );
        *retries = 1;
        let id = task.id;
        let weak = self.self_weak.clone();
        let _ = app.after(self.window, CANCEL_RETRY_DELAY, move |_app| {
            if let Some(core) = weak.upgrade() {
                core.borrow_mut().retry_cleanup(id);
            }
        });
    }

    fn retry_cleanup(&mut self, id: DownloadId) {
        if !self.cleanups.contains_key(&id) {
            return;
        }
        if !self.downloads.delete(id) {
            self.cleanups.remove(&id);
        }
    }

    fn read_payload(&mut self, task: DownloadId, destination: std::path::PathBuf) {
        if self.reading == Some(task) {
            return;
        }
        self.reading = Some(task);
        let runtime = self.runtime.clone();
        let wake = self.wake.clone();
        let inbox = Arc::clone(&self.inbox);
        let generation = self.generation;
        self.runtime.spawn(async move {
            let result = match runtime
                .spawn_blocking(move || std::fs::read(&destination))
                .await
            {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(error)) => Err(UpdaterError::Download(DownloadFailure::Io {
                    kind: error.kind(),
                })),
                Err(_) => Err(UpdaterError::Download(DownloadFailure::Task)),
            };
            push_message(
                &inbox,
                Message::PayloadRead {
                    generation,
                    task,
                    result,
                },
            );
            wake.wake();
        });
    }

    fn finish_payload(
        &mut self,
        app: &mut App,
        task: DownloadId,
        result: Result<Vec<u8>, UpdaterError>,
    ) {
        if self.reading == Some(task) {
            self.reading = None;
        }
        if self.download_task == Some(task) {
            self.download_task = None;
        }
        if self.downloads.delete(task) {
            self.cleanups.entry(task).or_insert(0);
        }
        match result {
            Ok(bytes) => {
                if self.state.download_finished(task, Bytes::from(bytes)) {
                    self.publish_view(app);
                }
            }
            Err(error) => {
                if self
                    .state
                    .download()
                    .is_some_and(|download| download.task == task)
                {
                    self.state.attempt_failed(error);
                    self.publish_view(app);
                } else {
                    log::debug!("the read of an update download that was given up failed: {error}");
                }
            }
        }
    }

    fn pause_download(&mut self, app: &mut App) {
        let UpdaterState::Downloading(download) = &self.state else {
            return;
        };
        let task = download.task;
        if self.downloads.pause(task) && self.state.pause_started() {
            self.publish_view(app);
        }
    }

    fn resume_download(&mut self, app: &mut App) {
        let UpdaterState::Paused(download) = &self.state else {
            return;
        };
        let task = download.task;
        if self.downloads.resume(task) && self.state.resume_started() {
            self.publish_view(app);
        }
    }

    fn cancel_download(&mut self, app: &mut App) {
        let Some(download) = self.state.download() else {
            return;
        };
        let task = download.task;
        if self.downloads.delete(task) {
            self.cleanups.insert(task, 0);
        }
        self.download_task = None;
        self.state.abandon_attempt();
        self.publish_view(app);
    }

    fn discard_download(&mut self, app: &mut App) {
        self.download_task = None;
        self.state.abandon_attempt();
        self.publish_view(app);
    }

    fn install(&mut self, app: &mut App) {
        if self.installing {
            return;
        }
        let Some((release, bytes)) = self.state.begin_install() else {
            return;
        };
        self.installing = true;
        self.publish_view(app);

        let language = *app.read(&self.services.language);
        let installing = install::install(&self.runtime, &release, bytes, language);
        let wake = self.wake.clone();
        let inbox = Arc::clone(&self.inbox);
        let generation = self.generation;
        self.runtime.spawn(async move {
            let result = match installing.await {
                Ok(result) => result,
                Err(error) => Err(InstallError::Task(error)),
            };
            push_message(&inbox, Message::Installed { generation, result });
            wake.wake();
        });
    }

    fn finish_install(&mut self, app: &mut App, result: Result<(), InstallError>) {
        match result {
            Ok(()) => {
                self.installing = false;
                quit(app);
            }
            Err(error) => {
                self.installing = false;
                self.state.attempt_failed(UpdaterError::Install(error));
                self.publish_view(app);
            }
        }
    }

    fn dismiss_failure(&mut self, app: &mut App) {
        self.state.failure_dismissed();
        self.publish_view(app);
    }

    fn sync_dialog(&mut self, app: &mut App) -> Result<(), EngineError> {
        let window = self.window;
        let Some(data) = self.dialog_data(app) else {
            let was_requested = std::mem::take(&mut self.dialog_requested);
            if let Some(dialog) = self.dialog.as_mut() {
                if was_requested {
                    dialog.close(app);
                }
                dialog.sync(app, window)?;
            }
            return Ok(());
        };
        if self.dialog_requested {
            if self.dialog.is_none() {
                let self_weak = self.self_weak.clone();
                let dialog = UpdateDialog::new(
                    app,
                    Rc::clone(&self.services),
                    format_smolstr!("{}.dialog", self.key),
                    self.view,
                    self_weak,
                );
                self.dialog = Some(dialog);
            }
            if let Some(dialog) = self.dialog.as_mut() {
                dialog.show(app, window, data)?;
            }
        }
        let mut still_open = false;
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.sync(app, window)?;
            still_open = dialog.is_open(app) || dialog.is_closing(app);
        }
        if !still_open {
            self.dialog_requested = false;
        }
        Ok(())
    }

    fn dialog_data(&self, app: &App) -> Option<DialogData> {
        let view = UpdaterView::of(&self.state)?;
        let release = self.state.release()?;
        let language = *app.read(&self.services.language);
        let notes = release
            .notes
            .as_ref()
            .and_then(|notes| notes.in_language(language))
            .cloned();
        Some(DialogData {
            view,
            title: t_smol!(&self.translate, "window.updater.available"),
            notes,
            language,
            install_supported: install::supported(),
        })
    }

    fn handle_control(&mut self, app: &mut App, control: DialogControl) {
        match control {
            DialogControl::Download => self.download(app),
            DialogControl::Pause => self.pause_download(app),
            DialogControl::Resume => self.resume_download(app),
            DialogControl::Install => self.install(app),
            DialogControl::Close => self.close_dialog(app),
            DialogControl::Cancel => {
                self.cancel_download(app);
                self.close_dialog(app);
            }
            DialogControl::Discard => {
                self.discard_download(app);
                self.close_dialog(app);
            }
            DialogControl::DismissFailure => {
                self.dismiss_failure(app);
                self.close_dialog(app);
            }
        }
    }

    fn close_dialog(&mut self, app: &mut App) {
        self.dialog_requested = false;
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.close(app);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DialogControl {
    Download,
    Pause,
    Resume,
    Cancel,
    Discard,
    Install,
    DismissFailure,
    Close,
}

fn quit(app: &mut App) {
    let windows = app.windows().map(|(window, _)| window).collect::<Vec<_>>();
    for window in windows {
        app.close_window(window);
    }
}

pub(crate) fn control_callback(
    updater: &Weak<RefCell<UpdaterCore>>,
    control: DialogControl,
) -> impl Fn(&mut ui_core::interaction::ActionCtx<'_>) + 'static {
    let updater = updater.clone();
    move |ctx| {
        let Some(core) = updater.upgrade() else {
            return;
        };
        core.borrow_mut().handle_control(ctx.app_mut(), control);
    }
}

#[cfg(test)]
pub(crate) trait UpdaterTestExt {
    fn test_core(&self) -> Rc<RefCell<UpdaterCore>>;

    fn test_set_state(&self, app: &mut App, state: UpdaterState);

    fn test_control(&self, app: &mut App, control: DialogControl);

    fn test_finish_check(&self, app: &mut App, result: Result<Option<ReleaseInfo>, UpdaterError>);

    fn test_finish_payload(
        &self,
        app: &mut App,
        task: DownloadId,
        result: Result<Vec<u8>, UpdaterError>,
    );

    fn test_finish_install(&self, app: &mut App, result: Result<(), InstallError>);

    fn test_cancel_check(&self, app: &mut App);

    fn test_download_task(&self) -> Option<DownloadId>;

    fn test_cleanups(&self) -> usize;

    fn test_failed_checks(&self) -> usize;

    fn test_has_scheduled_check(&self) -> bool;

    fn test_generation(&self) -> u64;

    fn test_push_checked(&self, generation: u64, result: Result<Option<ReleaseInfo>, UpdaterError>);

    fn test_dialog_open(&self, app: &App) -> bool;

    fn test_dialog_closing(&self, app: &App) -> bool;

    fn test_press(&self, app: &mut App, suffix: &str) -> bool;
}

#[cfg(test)]
impl UpdaterTestExt for Updater {
    fn test_core(&self) -> Rc<RefCell<UpdaterCore>> {
        Rc::clone(&self.core)
    }

    fn test_set_state(&self, app: &mut App, state: UpdaterState) {
        let mut core = self.core.borrow_mut();
        core.state = state;
        core.publish_view(app);
    }

    fn test_control(&self, app: &mut App, control: DialogControl) {
        self.core.borrow_mut().handle_control(app, control);
    }

    fn test_finish_check(&self, app: &mut App, result: Result<Option<ReleaseInfo>, UpdaterError>) {
        self.core.borrow_mut().finish_check(app, result);
    }

    fn test_finish_payload(
        &self,
        app: &mut App,
        task: DownloadId,
        result: Result<Vec<u8>, UpdaterError>,
    ) {
        self.core.borrow_mut().finish_payload(app, task, result);
    }

    fn test_finish_install(&self, app: &mut App, result: Result<(), InstallError>) {
        self.core.borrow_mut().finish_install(app, result);
    }

    fn test_cancel_check(&self, app: &mut App) {
        let mut core = self.core.borrow_mut();
        if let Some(timer) = core.check_timer.take() {
            app.cancel_timer(&timer);
        }
    }

    fn test_download_task(&self) -> Option<DownloadId> {
        self.core.borrow().download_task
    }

    fn test_cleanups(&self) -> usize {
        self.core.borrow().cleanups.len()
    }

    fn test_failed_checks(&self) -> usize {
        self.core.borrow().failed_checks
    }

    fn test_has_scheduled_check(&self) -> bool {
        self.core.borrow().check_timer.is_some()
    }

    fn test_generation(&self) -> u64 {
        self.core.borrow().generation
    }

    fn test_push_checked(
        &self,
        generation: u64,
        result: Result<Option<ReleaseInfo>, UpdaterError>,
    ) {
        push_message(
            &self.core.borrow().inbox,
            Message::Checked { generation, result },
        );
    }

    fn test_dialog_open(&self, app: &App) -> bool {
        self.core
            .borrow()
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(app))
    }

    fn test_dialog_closing(&self, app: &App) -> bool {
        self.core
            .borrow()
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_closing(app))
    }

    fn test_press(&self, app: &mut App, suffix: &str) -> bool {
        let (window, node, action) = {
            let core = self.core.borrow();
            let Some(dialog) = core.dialog.as_ref() else {
                return false;
            };
            let built = dialog.built.borrow();
            let Some(built) = built.as_ref() else {
                return false;
            };
            let Some(button) = built
                .buttons
                .iter()
                .find(|button| button.key().ends_with(&format!(".{suffix}")))
            else {
                return false;
            };
            let (Some(node), Some(action)) = (button.node(), button.action_identity()) else {
                return false;
            };
            (core.window, node, action)
        };
        app.dispatch_action(
            window,
            node,
            action,
            ui_core::event::ActionSource::Programmatic,
        )
    }
}

#[cfg(test)]
mod tests {

    use download_provider::{
        error::DownloadFailure,
        task::{DownloadId, DownloadStatus},
    };

    use super::*;
    use crate::{
        error::{InstallError, UpdaterError},
        state::UpdaterState,
        test_support::{Harness, release},
    };

    fn installed() -> (Harness, Updater) {
        let mut harness = Harness::new();
        let updater = harness.install_updater(None);
        updater.test_cancel_check(harness.app.app_mut());
        (harness, updater)
    }

    fn view(harness: &Harness, updater: &Updater) -> Option<UpdaterView> {
        updater.view(harness.app.app())
    }

    #[test]
    fn installing_should_subscribe_to_the_manager_and_schedule_the_first_check() {
        let mut harness = Harness::new();
        let baseline_slots = harness.signal_slot_stats();
        let baseline_routes = harness.router.len();
        let updater = harness.install_updater(None);

        assert_eq!(
            harness.downloads().subscriptions(),
            1,
            "the updater follows the download manager"
        );
        assert_eq!(
            harness.router.len(),
            baseline_routes + 1,
            "the updater registers its one frame route"
        );
        assert!(
            updater.test_has_scheduled_check(),
            "the first check is scheduled rather than run at install"
        );
        assert!(view(&harness, &updater).is_none(), "nothing is offered yet");
        assert!(
            harness.signal_slot_stats().live > baseline_slots.live,
            "the updater mints the view signal it shows"
        );

        assert!(
            updater.release(harness.app.app_mut()) > 0,
            "releasing gives the value scope back"
        );
        assert!(
            !updater.test_has_scheduled_check(),
            "releasing takes the scheduled check with it"
        );
        assert_eq!(
            harness.downloads().subscriptions(),
            0,
            "releasing withdraws the manager subscription"
        );
        assert_eq!(
            harness.router.len(),
            baseline_routes,
            "releasing unregisters the frame route"
        );
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline_slots.live,
            "releasing reclaims every value the updater minted"
        );
    }

    #[test]
    fn a_check_that_finds_nothing_should_leave_nothing_on_offer() {
        let (mut harness, updater) = installed();

        updater.test_finish_check(harness.app.app_mut(), Ok(None));
        harness.drain();

        assert_eq!(view(&harness, &updater), None);
        assert_eq!(updater.test_failed_checks(), 0);
    }

    #[test]
    fn a_check_that_finds_a_release_should_offer_it() {
        let (mut harness, updater) = installed();

        updater.test_finish_check(harness.app.app_mut(), Ok(Some(release("1.2.3"))));
        harness.drain();

        assert_eq!(view(&harness, &updater), Some(UpdaterView::Notes));
        assert!(UpdaterView::Notes.offered());
    }

    #[test]
    fn a_failed_check_should_be_counted_and_scheduled_until_the_attempts_run_out() {
        let (mut harness, updater) = installed();

        for spent in 1..=4 {
            updater.test_finish_check(
                harness.app.app_mut(),
                Err(UpdaterError::Transport(
                    crate::error::UpdateRequestError::Cancelled,
                )),
            );

            assert_eq!(updater.test_failed_checks(), spent);
            assert_eq!(
                view(&harness, &updater),
                None,
                "a failed check offers nothing"
            );
            assert_eq!(
                updater.test_has_scheduled_check(),
                retry_delay(spent - 1).is_some(),
                "the check waiting after {spent} failures is the one the schedule has"
            );
        }
    }

    #[test]
    fn a_result_should_only_be_applied_by_the_generation_that_asked() {
        let (mut harness, updater) = installed();
        let generation = updater.test_generation();

        updater.test_push_checked(generation + 1, Ok(Some(release("1.2.3"))));
        harness.drain();
        assert_eq!(
            view(&harness, &updater),
            None,
            "a result from a generation that is not current is dropped"
        );

        updater.test_push_checked(generation, Ok(Some(release("1.2.3"))));
        harness.drain();
        assert_eq!(view(&harness, &updater), Some(UpdaterView::Notes));
    }

    #[test]
    fn a_steady_frame_should_write_nothing_and_ask_for_nothing() {
        let (mut harness, updater) = installed();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        harness.settle(2);
        let requests = harness.app.app().frame_requests();

        harness.settle(3);

        assert_eq!(
            harness.app.app().frame_requests(),
            requests,
            "a steady updater requests no frames (I3)"
        );
        assert_eq!(view(&harness, &updater), Some(UpdaterView::Notes));
    }

    #[test]
    fn a_download_should_add_a_task_and_follow_its_report() {
        let (mut harness, updater) = installed();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        updater.test_control(harness.app.app_mut(), DialogControl::Download);

        let task = harness
            .downloads()
            .last_task()
            .expect("starting a download adds a task");
        assert_eq!(updater.test_download_task(), Some(task));
        assert_eq!(
            view(&harness, &updater),
            Some(UpdaterView::Download {
                downloaded: 0,
                total: None,
                phase: DownloadPhase::Downloading,
            })
        );
        let request = harness.downloads().requests().remove(0);
        assert_eq!(
            request.url,
            release("1.2.3").asset.url,
            "the task downloads the payload the release named"
        );
        assert_eq!(request.class, HttpClientClass::UpdaterDownload);

        harness.downloads().publish(
            task,
            DownloadStatus::Downloading {
                downloaded: 40,
                total: Some(100),
                mb_per_second: 1.0,
            },
        );
        harness.drain();

        let view = view(&harness, &updater).expect("the download is drawn");
        assert_eq!(view.percent(), Some(40.0), "the bar follows the report");
        assert_eq!(view.phase(), Some(DownloadPhase::Downloading));
    }

    #[test]
    fn a_failed_transfer_should_fail_the_attempt_and_keep_the_release_offered() {
        let (mut harness, updater) = installed();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        updater.test_control(harness.app.app_mut(), DialogControl::Download);
        let task = harness.downloads().last_task().expect("a task was added");

        harness
            .downloads()
            .fail(task, DownloadFailure::Status { status: 404 });
        harness.drain();

        match view(&harness, &updater) {
            Some(UpdaterView::Failure(key)) => {
                assert_eq!(key, "error.updater.download");
            }
            other => panic!("a failed transfer is a failure view, not {other:?}"),
        }
        assert!(
            updater.test_core().borrow().state.offered().is_some(),
            "the release stays on offer after a failed attempt"
        );
    }

    #[test]
    fn pausing_and_resuming_should_follow_the_manager() {
        let (mut harness, updater) = installed();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        updater.test_control(harness.app.app_mut(), DialogControl::Download);
        let task = harness.downloads().last_task().expect("a task was added");

        updater.test_control(harness.app.app_mut(), DialogControl::Pause);
        harness.drain();
        assert_eq!(
            view(&harness, &updater).and_then(UpdaterView::phase),
            Some(DownloadPhase::Pausing),
            "the manager accepted the pause"
        );

        harness.downloads().publish(
            task,
            DownloadStatus::Paused {
                downloaded: 40,
                total: Some(100),
            },
        );
        harness.drain();
        assert_eq!(
            view(&harness, &updater).and_then(UpdaterView::phase),
            Some(DownloadPhase::Paused)
        );

        updater.test_control(harness.app.app_mut(), DialogControl::Resume);
        harness.drain();
        assert_eq!(
            view(&harness, &updater).and_then(UpdaterView::phase),
            Some(DownloadPhase::Downloading),
            "a stopped download continues from its partial file"
        );
    }

    #[test]
    fn cancelling_should_offer_the_release_again_and_delete_the_task() {
        let (mut harness, updater) = installed();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        updater.test_control(harness.app.app_mut(), DialogControl::Download);
        let task = harness.downloads().last_task().expect("a task was added");

        updater.test_control(harness.app.app_mut(), DialogControl::Cancel);
        harness.drain();

        assert_eq!(view(&harness, &updater), Some(UpdaterView::Notes));
        assert_eq!(updater.test_download_task(), None);
        assert!(
            harness.downloads().deleted().contains(&task),
            "the manager was asked to remove the task and its partial file"
        );
        assert_eq!(
            updater.test_cleanups(),
            0,
            "the removal settled, so nothing is being followed"
        );
    }

    #[test]
    fn a_completed_download_should_be_read_and_offered_for_installation() {
        let (mut harness, updater) = installed();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        updater.test_control(harness.app.app_mut(), DialogControl::Download);
        let task = harness.downloads().last_task().expect("a task was added");

        harness.downloads().complete(task, b"payload");
        harness.wait_until("the payload to be read", |harness| {
            updater.view(harness.app.app()) == Some(UpdaterView::Downloaded)
        });
        assert!(
            harness.downloads().deleted().contains(&task),
            "reading the payload removes the task that owned its file"
        );

        updater.test_control(harness.app.app_mut(), DialogControl::Discard);
        harness.drain();
        assert_eq!(
            view(&harness, &updater),
            Some(UpdaterView::Notes),
            "giving the payload up offers the release again"
        );
    }

    #[test]
    fn a_payload_read_that_arrives_after_the_download_was_given_up_should_be_dropped() {
        let (mut harness, updater) = installed();
        let task = DownloadId::new();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::downloading_for_test(release("1.2.3"), task, 4_096, Some(8_192)),
        );

        updater.test_control(harness.app.app_mut(), DialogControl::Cancel);
        harness.drain();
        assert_eq!(view(&harness, &updater), Some(UpdaterView::Notes));

        updater.test_finish_payload(harness.app.app_mut(), task, Ok(b"payload".to_vec()));
        harness.drain();
        assert_eq!(
            view(&harness, &updater),
            Some(UpdaterView::Notes),
            "a payload for a download that was given up is not this attempt's"
        );
    }

    #[test]
    fn a_failed_install_should_be_reported_the_way_a_failed_download_is() {
        let (mut harness, updater) = installed();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::ready_for_test(release("1.2.3"), Bytes::from_static(b"payload")),
        );

        updater.test_finish_install(harness.app.app_mut(), Err(InstallError::ExtractPath));
        harness.drain();

        match view(&harness, &updater) {
            Some(UpdaterView::Failure(key)) => assert_eq!(key, "error.updater.install"),
            other => panic!("a failed install is a failure view, not {other:?}"),
        }

        updater.test_control(harness.app.app_mut(), DialogControl::DismissFailure);
        harness.drain();
        assert_eq!(view(&harness, &updater), Some(UpdaterView::Notes));
    }

    #[test]
    fn a_finished_install_should_end_the_application() {
        let (mut harness, updater) = installed();
        let window = harness.window();
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Installing(release("1.2.3")),
        );
        assert_eq!(
            view(&harness, &updater),
            Some(UpdaterView::Installing),
            "the dialog has nothing left to press"
        );

        updater.test_finish_install(harness.app.app_mut(), Ok(()));

        assert!(
            harness.app.app().window(window).is_none(),
            "an installed update closes the application (which starts the installed one)"
        );
    }

    #[test]
    fn releasing_the_updater_should_reclaim_every_value_and_its_subscription() {
        let mut harness = Harness::new();
        let baseline = harness.signal_slot_stats();
        let routes = harness.router.len();
        let updater = harness.install_updater(None);

        updater.release(harness.app.app_mut());
        updater.release(harness.app.app_mut());

        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "releasing reclaims every value the updater minted"
        );
        assert_eq!(harness.router.len(), routes);
        assert_eq!(harness.downloads().subscriptions(), 0);
    }
}

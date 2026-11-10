//! The updater entity: what is known about a newer release, and what is being done about it.
//!
//! Every transition lives on [`UpdaterState`] and is pure, so the whole machine —
//! what a check offers, what a download does to it, what cancelling leaves behind —
//! is exercised without a network. The entity around it owns the requests, the
//! schedule they run on, and the notifications the interface repaints from.

use std::{path::PathBuf, sync::Arc, time::Duration};

use ahash::AHashMap;
use app_config::network::{Config, KytrixServiceCapability, RouteTarget, ServiceTarget};
use app_config_provider::notification::ConfigNotification;
use bytes::Bytes;
use download_provider::{
    error::DownloadFailure,
    provider::DownloadProvider,
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadStatus, DownloadTask},
};
use gpui_kit::{Context, Entity, Subscription, Task};
use i18n::language::codes::LanguageCode;
use network::transport::{http::HttpClientClass, hub::TransportHub};
use smol_str::format_smolstr;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use crate::{
    error::{InstallError, MetadataError, UpdateRequestError, UpdaterError},
    fetch,
    global::UpdaterDeps,
    install, publish,
    release::ReleaseInfo,
};

/// How long after startup the first check runs.
///
/// Startup is already a burst of work — settings, the window, the first market
/// data — and an update check competing with it buys nothing: nobody is waiting
/// for the answer.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(5);

/// How long the retry that follows a failed check waits, by the checks already failed.
///
/// A check that fails says nothing to the user: the button appearing is the whole
/// of what this feature reports, so a failure is retried quietly a few times —
/// startup is exactly when a machine may have no usable network yet — and then
/// left until the next launch.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
];

/// How long the retry after `spent` failed checks waits, or `None` when this
/// launch has spent its attempts.
fn retry_delay(spent: usize) -> Option<Duration> {
    RETRY_DELAYS.get(spent).copied()
}

/// How long the retry after a failed delete of a download this entity gave up
/// waits.
///
/// A delete fails when the attempt that owns the file is still writing it. A retry
/// a moment later is enough to outlast that write, and what is at stake is a task
/// and a temporary file rather than the download the user gave up, so one retry is
/// all this asks for.
const CANCEL_RETRY_DELAY: Duration = Duration::from_millis(500);

/// What the updater knows and what it is doing about it.
#[derive(Debug)]
pub(crate) enum UpdaterState {
    /// Nothing newer is known, and no check is running.
    Idle,
    /// The metadata request is in flight.
    Checking,
    /// A newer release is available and nothing is being done about it yet.
    Available(ReleaseInfo),
    /// The payload of the available release is being downloaded.
    Downloading(Download),
    /// The transfer is being stopped and will settle as [`Self::Paused`].
    Pausing(Download),
    /// The transfer is stopped at its partial file and can be resumed.
    Paused(Download),
    /// The payload is downloaded, waiting for the user.
    ReadyToInstall(DownloadedUpdate),
    /// The payload is being installed.
    Installing(ReleaseInfo),
    /// The last attempt failed; the dialog says so, and the release stays on offer.
    Failed(FailedUpdate),
}

/// A download in flight.
#[derive(Clone, Debug)]
pub(crate) struct Download {
    release: ReleaseInfo,
    /// The download manager's task, which is what names this transfer's events.
    task: DownloadId,
    /// Bytes accepted so far; it does not decrease, except when the server answers
    /// a resumed `Range` request with `200` and the transfer starts over from zero.
    downloaded: u64,
    /// The exact payload size, when the response named one.
    total: Option<u64>,
}

impl Download {
    /// What the response has handed over so far.
    pub(crate) fn accepted(&self) -> u64 {
        self.downloaded
    }

    /// The size the response named, when it named one.
    ///
    /// A chunked answer has no length to draw a bar against, so what reports it draws
    /// something else.
    pub(crate) fn named_total(&self) -> Option<u64> {
        self.total
    }
}

/// A payload that was downloaded, waiting to be installed.
#[derive(Debug)]
pub(crate) struct DownloadedUpdate {
    release: ReleaseInfo,
    bytes: Bytes,
}

/// A failed attempt, with the release it was about so it can be tried again.
#[derive(Debug)]
pub(crate) struct FailedUpdate {
    release: ReleaseInfo,
    error: UpdaterError,
}

impl UpdaterState {
    /// The release this state is about, when one is known.
    pub(crate) fn release(&self) -> Option<&ReleaseInfo> {
        match self {
            Self::Available(release)
            | Self::Downloading(Download { release, .. })
            | Self::Pausing(Download { release, .. })
            | Self::Paused(Download { release, .. })
            | Self::ReadyToInstall(DownloadedUpdate { release, .. })
            | Self::Installing(release)
            | Self::Failed(FailedUpdate { release, .. }) => Some(release),
            Self::Idle | Self::Checking => None,
        }
    }

    /// The release the interface offers, which is one no attempt is in flight for.
    pub(crate) fn offered(&self) -> Option<&ReleaseInfo> {
        match self {
            Self::Available(release) | Self::Failed(FailedUpdate { release, .. }) => Some(release),
            _ => None,
        }
    }

    /// The failure the dialog speaks for, when the last attempt failed.
    pub(crate) fn failure(&self) -> Option<&UpdaterError> {
        match self {
            Self::Failed(FailedUpdate { error, .. }) => Some(error),
            _ => None,
        }
    }

    /// The download this state is about, while one of the download states is current.
    fn download(&self) -> Option<&Download> {
        match self {
            Self::Downloading(download) | Self::Pausing(download) | Self::Paused(download) => {
                Some(download)
            }
            _ => None,
        }
    }

    /// Starts a check, unless one is already running.
    fn begin_check(&mut self) -> bool {
        if matches!(self, Self::Checking) {
            return false;
        }
        *self = Self::Checking;

        true
    }

    /// Applies what a finished check found.
    fn check_finished(&mut self, release: Option<ReleaseInfo>) {
        *self = match release {
            Some(release) => Self::Available(release),
            None => Self::Idle,
        };
    }

    /// Starts a download of the offered release, which is what it returns.
    fn begin_download(&mut self, task: DownloadId) -> Option<ReleaseInfo> {
        let release = self.offered()?.clone();
        *self = Self::Downloading(Download {
            release: release.clone(),
            task,
            downloaded: 0,
            total: None,
        });

        Some(release)
    }

    /// Records what the transfer reported; `true` when what is drawn moved.
    ///
    /// Only the transfer in flight takes a report: a report for a task the state no
    /// longer holds is not about this download.
    fn record_progress(&mut self, task: DownloadId, downloaded: u64, total: Option<u64>) -> bool {
        let Self::Downloading(download) = self else {
            return false;
        };
        if download.task != task {
            return false;
        }
        let before = (download.downloaded, download.total);
        // The transfer reports what it has written, so a number below what is
        // already known is a stale report rather than news — except zero, which is
        // the server restarting the entity it would not range. The bar has to
        // follow that restart down, or it stays where the abandoned attempt left
        // it while the new one starts over underneath.
        if downloaded == 0 || downloaded >= download.downloaded {
            download.downloaded = downloaded;
        }
        if total.is_some() {
            download.total = total;
        }

        (download.downloaded, download.total) != before
    }

    /// The transfer is being stopped; it settles as [`Self::Paused`] once it has.
    fn pause_started(&mut self) -> bool {
        let Self::Downloading(download) = self else {
            return false;
        };
        let download = download.clone();
        *self = Self::Pausing(download);

        true
    }

    /// The stopped transfer keeps its partial file, which is what a resume continues.
    ///
    /// `downloaded` and `total` are what the transfer itself reports as stopped: a
    /// chunk that was still in flight when the pause was asked for lands in the
    /// numbers the provider publishes with [`DownloadStatus::Paused`], not in the
    /// ones this state had at the click.
    fn pause_finished(&mut self, downloaded: u64, total: Option<u64>) -> bool {
        let Self::Pausing(download) = self else {
            return false;
        };
        let mut download = download.clone();
        download.downloaded = downloaded;
        if total.is_some() {
            download.total = total;
        }
        *self = Self::Paused(download);

        true
    }

    /// The stopped transfer started again from its partial file.
    fn resume_started(&mut self) -> bool {
        let Self::Paused(download) = self else {
            return false;
        };
        let download = download.clone();
        *self = Self::Downloading(download);

        true
    }

    /// Applies a payload that arrived.
    ///
    /// Only the download this attempt still owns becomes ready to install: a payload
    /// that arrives after the user cancelled, or after the state moved on, is dropped
    /// here. A download being stopped, or already stopped, still takes its payload:
    /// the file is complete whichever of the two reported last.
    fn download_finished(&mut self, task: DownloadId, bytes: Bytes) -> bool {
        let (Self::Downloading(download) | Self::Pausing(download) | Self::Paused(download)) = self
        else {
            return false;
        };
        if download.task != task {
            return false;
        }
        let release = download.release.clone();
        *self = Self::ReadyToInstall(DownloadedUpdate { release, bytes });

        true
    }

    /// Moves to installing and hands out what the installer needs, once.
    fn begin_install(&mut self) -> Option<(ReleaseInfo, Bytes)> {
        let Self::ReadyToInstall(update) = self else {
            return None;
        };
        let release = update.release.clone();
        let bytes = update.bytes.clone();
        *self = Self::Installing(release.clone());

        Some((release, bytes))
    }

    /// The attempt failed: the release stays on offer, and the dialog says why.
    fn attempt_failed(&mut self, error: UpdaterError) {
        let Some(release) = self.release().cloned() else {
            return;
        };
        *self = Self::Failed(FailedUpdate { release, error });
    }

    /// The attempt is over without installing anything: whatever it produced is
    /// dropped and the release is on offer again.
    ///
    /// Cancelling a download and closing the dialog on a downloaded payload both land
    /// here, which is what makes each of them safe to reach twice.
    fn abandon_attempt(&mut self) {
        let Some(release) = self.release().cloned() else {
            return;
        };
        *self = Self::Available(release);
    }

    /// The dialog closed on a failure: the release is on offer again.
    fn failure_dismissed(&mut self) {
        let Self::Failed(FailedUpdate { release, .. }) = self else {
            return;
        };
        *self = Self::Available(release.clone());
    }

    /// A download in flight, for a test that draws one without a network.
    #[cfg(test)]
    pub(crate) fn downloading_for_test(
        release: ReleaseInfo,
        task: DownloadId,
        downloaded: u64,
        total: Option<u64>,
    ) -> Self {
        Self::Downloading(Download {
            release,
            task,
            downloaded,
            total,
        })
    }

    /// A download being stopped, for a test that draws one without a network.
    #[cfg(test)]
    pub(crate) fn pausing_for_test(
        release: ReleaseInfo,
        task: DownloadId,
        downloaded: u64,
        total: Option<u64>,
    ) -> Self {
        Self::Pausing(Download {
            release,
            task,
            downloaded,
            total,
        })
    }

    /// A stopped download, for a test that draws one without a network.
    #[cfg(test)]
    pub(crate) fn paused_for_test(
        release: ReleaseInfo,
        task: DownloadId,
        downloaded: u64,
        total: Option<u64>,
    ) -> Self {
        Self::Paused(Download {
            release,
            task,
            downloaded,
            total,
        })
    }

    /// A payload that was downloaded, for a test that draws one without a network.
    #[cfg(test)]
    pub(crate) fn ready_for_test(release: ReleaseInfo, bytes: Bytes) -> Self {
        Self::ReadyToInstall(DownloadedUpdate { release, bytes })
    }

    /// A failed attempt, for a test that draws one without a network.
    #[cfg(test)]
    pub(crate) fn failed_for_test(release: ReleaseInfo, error: UpdaterError) -> Self {
        Self::Failed(FailedUpdate { release, error })
    }
}

/// The updater: the state machine, the requests it runs, and the schedule they run on.
pub(crate) struct Updater {
    transport: Arc<TransportHub>,
    provider: Entity<DownloadProvider>,
    config: ConfigNotification<Config>,
    runtime: Runtime,
    state: UpdaterState,
    /// The check in flight, or the one waiting for its delay.
    ///
    /// `Some` exactly while a check is scheduled or running, so what it holds is
    /// also what says whether this launch still has an attempt left.
    check: Option<Task<()>>,
    /// The install in flight: dropping its task would cancel the installer.
    installing: Option<Task<()>>,
    /// The read of a completed download's temporary file.
    ///
    /// Held for as long as it runs because a dropped GPUI task is a cancelled one,
    /// and this task is what both hands the payload to the state and removes the
    /// file; a later download replaces it.
    reading: Option<Task<()>>,
    /// The provider task of the attempt the state is about.
    ///
    /// It is kept when an attempt fails, because the retry resumes that task and
    /// its partial file instead of adding a second transfer; it is cleared as soon
    /// as the attempt ends with nothing left to resume — cancelled, discarded, or
    /// read into the payload.
    download_task: Option<DownloadId>,
    /// The tasks whose removal this entity is following, each with how many times
    /// its delete was already asked for again.
    ///
    /// Cancelling a download puts the release straight back on offer, and reading a
    /// payload gives the download up entirely, so nothing else in this entity still
    /// refers to those tasks; each is followed only so that a delete that failed is
    /// not a task and a temporary file left with nobody to remove them. More than
    /// one can be outstanding at once — the read of a payload can start while an
    /// earlier cancel is still retrying — so every removal is tracked on its own
    /// rather than one slot standing for all of them.
    cleanups: AHashMap<DownloadId, u8>,
    /// The download manager subscription, held because dropping it would stop the
    /// events the dialog follows.
    provider_events: Option<Subscription>,
    /// Failed checks spent since the application started, which the backoff counts.
    failed_checks: usize,
}

impl Updater {
    /// The updater over the composition root's dependencies.
    pub(crate) fn new(deps: UpdaterDeps, config: ConfigNotification<Config>) -> Self {
        Self {
            transport: deps.transport,
            provider: deps.provider,
            config,
            runtime: deps.runtime,
            state: UpdaterState::Idle,
            check: None,
            installing: None,
            reading: None,
            download_task: None,
            cleanups: AHashMap::default(),
            provider_events: None,
            failed_checks: 0,
        }
    }

    /// Follows the download manager, so a transfer's events are what this state moves on.
    ///
    /// Watching twice would deliver every event twice, so the first subscription stays.
    pub(crate) fn watch_provider(&mut self, cx: &mut Context<Self>) {
        if self.provider_events.is_some() {
            return;
        }
        let provider = self.provider.clone();
        self.provider_events = Some(cx.subscribe(&provider, |this, _, event, cx| {
            this.download_event(event, cx);
        }));
    }

    /// Starts this launch's update schedule.
    pub(crate) fn start(&mut self, cx: &mut Context<Self>) {
        self.schedule_check(FIRST_CHECK_DELAY, cx);
    }

    /// What the interface draws.
    pub(crate) fn state(&self) -> &UpdaterState {
        &self.state
    }

    /// The network configuration this updater routes its requests with.
    #[cfg(test)]
    pub(crate) fn config_for_test(&self) -> &ConfigNotification<Config> {
        &self.config
    }

    /// Asks the metadata endpoint whether a newer release exists.
    fn check_now(&mut self, cx: &mut Context<Self>) {
        if !self.state.begin_check() {
            return;
        }
        cx.notify();

        // The endpoint is compiled in, so a wrong one is a build defect; it is
        // still answered as a failed check rather than a panic, because that is
        // what the schedule around this already knows how to handle.
        let endpoint = match Url::parse(publish::METADATA_ENDPOINT) {
            Ok(endpoint) => endpoint,
            Err(source) => {
                self.finish_check(
                    Err(UpdaterError::Metadata(MetadataError::Endpoint(source))),
                    cx,
                );
                return;
            }
        };
        let transport = Arc::clone(&self.transport);
        let config = self.config.current();
        let runtime = self.runtime.clone();
        let resolving = runtime.clone();
        let cancellation = self.transport.cancellation().child_token();
        let checked = runtime.run(async move {
            fetch::fetch_release(&transport, &config, &resolving, &endpoint, &cancellation).await
        });

        self.check = Some(cx.spawn(async move |updater, cx| {
            let result = match checked.await {
                Ok(result) => result,
                Err(error) => Err(UpdaterError::Transport(UpdateRequestError::Task(error))),
            };
            let _ = updater.update(cx, |updater, cx| updater.finish_check(result, cx));
        }));
    }

    /// Applies what a check found, and schedules the next one when it failed.
    fn finish_check(
        &mut self,
        result: Result<Option<ReleaseInfo>, UpdaterError>,
        cx: &mut Context<Self>,
    ) {
        // The check is over, whichever way it went; a retry puts something back
        // here, so what the field holds is exactly the check that is still to run.
        self.check = None;
        match result {
            Ok(release) => {
                self.failed_checks = 0;
                self.state.check_finished(release);
            }
            Err(error) => {
                // Nobody asked for this update, so nobody is told it could not be
                // looked for: the button appearing is the whole of its report. What
                // is left is to try again a few times, in case this is a machine
                // whose network is not up yet.
                log::debug!("the update check failed: {error}");
                self.state = UpdaterState::Idle;
                let retry = retry_delay(self.failed_checks);
                self.failed_checks += 1;
                if let Some(delay) = retry {
                    self.schedule_check(delay, cx);
                }
            }
        }
        cx.notify();
    }

    /// Runs a check once `delay` has passed.
    fn schedule_check(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.check = Some(cx.spawn(async move |updater, cx| {
            cx.background_executor().timer(delay).await;
            let _ = updater.update(cx, |updater, cx| updater.check_now(cx));
        }));
    }

    /// Downloads the offered release through the download manager.
    ///
    /// An earlier attempt that failed is retried rather than replaced: the manager
    /// kept its task and partial file, so the retry continues the bytes that already
    /// arrived instead of fetching the payload again into a second temporary file.
    pub(crate) fn download(&mut self, cx: &mut Context<Self>) {
        // A platform that cannot install a payload does not fetch one: it would
        // hold the whole package on disk for a button it does not have.
        if !install::supported() {
            return;
        }
        let Some(release) = self.state.offered().cloned() else {
            return;
        };
        if let Some(task) = self.download_task {
            match self.provider.read(cx).task(task).map(|task| task.status) {
                Some(DownloadStatus::Paused { .. } | DownloadStatus::Failed { .. }) => {
                    // The manager still owns this attempt's partial file, so the
                    // retry resumes it; `begin_download` is what moves the state on
                    // from the failure, or from the offer it was dismissed back to.
                    if self
                        .provider
                        .update(cx, |provider, cx| provider.resume(task, cx))
                        && self.state.begin_download(task).is_some()
                    {
                        cx.notify();
                    }
                    return;
                }
                Some(DownloadStatus::Downloading { .. } | DownloadStatus::Pausing { .. }) => {
                    // The transfer this attempt started is still in flight; a second
                    // one would write a second file.
                    return;
                }
                Some(DownloadStatus::Completed { .. }) => {
                    // The payload is here, and the read of its file is what finishes
                    // this download; fetching it again would replace what is being read.
                    return;
                }
                Some(DownloadStatus::Deleting) | None => self.download_task = None,
            }
        }
        // A path of this attempt's own: a destination two tasks shared would let one
        // of them assemble its payload into the other's file.
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
        let task = self
            .provider
            .update(cx, |provider, cx| provider.add(request, cx));
        // The manager publishes the task while it is added, before this state knows
        // the task is its own, so the addition is not what moves the state; the
        // event handler ignores it and the download starts here.
        if self.state.begin_download(task).is_none() {
            log::debug!("the offered update disappeared while its download was being added");
            self.provider.update(cx, |provider, cx| {
                provider.delete(task, cx);
            });
            self.download_task = None;
            return;
        }
        self.download_task = Some(task);
        cx.notify();
    }

    /// Applies what the download manager reported about one of its tasks.
    ///
    /// Every event names its task: a report for a task this state no longer holds —
    /// a download that was cancelled or replaced — is not about this dialog.
    pub(crate) fn download_event(&mut self, event: &DownloadEvent, cx: &mut Context<Self>) {
        match event {
            // The task is gone, which is what giving it up asked for: there is
            // nothing left to follow for it.
            DownloadEvent::TaskRemoved(id) if self.cleanups.remove(id).is_some() => {
                return;
            }
            // The task this entity gave up is not the dialog's download any more;
            // what is left to do for it is getting its task and temporary file
            // removed.
            DownloadEvent::TaskUpdated(task) if self.cleanups.contains_key(&task.id) => {
                self.cleanup_failed(task, cx);
                return;
            }
            _ => {}
        }
        let DownloadEvent::TaskUpdated(task) = event else {
            // A task that left is a delete that settled. Cancelling has already put
            // the release back on offer, and a completed read has already taken the
            // file it needed.
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
                self.record_progress(task.id, *downloaded, *total, cx);
            }
            DownloadStatus::Pausing { .. } => {
                if self.state.pause_started() {
                    cx.notify();
                }
            }
            DownloadStatus::Paused { downloaded, total } => {
                if self.state.pause_finished(*downloaded, *total) {
                    cx.notify();
                }
            }
            DownloadStatus::Completed { .. } => {
                self.read_payload(task.id, task.destination.clone(), cx);
            }
            DownloadStatus::Failed { failure } => {
                // The manager keeps a failed task and its partial file, so
                // `download_task` stays: the next download resumes it.
                self.state.attempt_failed(UpdaterError::Download(*failure));
                cx.notify();
            }
            DownloadStatus::Deleting => {}
        }
    }

    /// Follows the delete of a task this entity gave up, which the manager answered
    /// with a failure.
    ///
    /// The dialog has already moved on by the time this runs — the release is on
    /// offer again, or the payload is waiting to install — so nothing here may touch
    /// what it draws: what a failure costs is a task the manager keeps and a
    /// temporary file nobody removes, which one more delete can still fix. A second
    /// failure is left in the log rather than retried forever, and only for the task
    /// that failed: another removal being followed still has its own retry.
    fn cleanup_failed(&mut self, task: &DownloadTask, cx: &mut Context<Self>) {
        let DownloadStatus::Failed { failure } = task.status else {
            // A delete still in flight, or a status only a bug could publish here:
            // the removal or its failure is still to come.
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
        cx.spawn(async move |updater, cx| {
            cx.background_executor().timer(CANCEL_RETRY_DELAY).await;
            let _ = updater.update(cx, |updater, cx| updater.retry_cleanup(id, cx));
        })
        .detach();
    }

    /// Asks the manager to delete a task this entity follows once more, if it still
    /// holds it.
    fn retry_cleanup(&mut self, id: DownloadId, cx: &mut Context<Self>) {
        if !self.cleanups.contains_key(&id) {
            return;
        }
        if !self
            .provider
            .update(cx, |provider, cx| provider.delete(id, cx))
        {
            // The manager no longer holds the task, so its files went with it.
            self.cleanups.remove(&id);
        }
    }

    /// Records the bytes a download has accepted; `total` is `None` while the
    /// response has not named one.
    fn record_progress(
        &mut self,
        task: DownloadId,
        downloaded: u64,
        total: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if self.state.record_progress(task, downloaded, total) {
            cx.notify();
        }
    }

    /// Reads a completed download's file and removes the task that owns it.
    ///
    /// The installer takes bytes rather than a path, so the file is read into memory
    /// first; either way the file has done its job afterwards, which is why the task
    /// and its file are removed whatever the read produced. The removal is followed
    /// like a cancel's, so a delete the file system refuses is retried once instead
    /// of leaving the task and its temporary file behind.
    fn read_payload(&mut self, task: DownloadId, destination: PathBuf, cx: &mut Context<Self>) {
        let reading = self
            .runtime
            .clone()
            .blocking(move || std::fs::read(&destination));
        self.reading = Some(cx.spawn(async move |updater, cx| {
            let result = match reading.await {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(error)) => Err(UpdaterError::Download(DownloadFailure::Io {
                    kind: error.kind(),
                })),
                Err(_) => Err(UpdaterError::Download(DownloadFailure::Task)),
            };
            let _ = updater.update(cx, |updater, cx| {
                let deleting = updater
                    .provider
                    .update(cx, |provider, cx| provider.delete(task, cx));
                // The payload has been read, so the task and its file are only a
                // cleanup now: following the delete is what makes a refused one a
                // retried removal rather than a task and a file nobody owns. A
                // delete already being followed keeps its retry count, so accepting
                // the same task again cannot buy it another retry.
                if deleting {
                    updater.cleanups.entry(task).or_insert(0);
                }
                updater.finish_payload(task, result, cx);
            });
        }));
    }

    /// Applies what reading a completed download produced.
    ///
    /// A read that finishes after the user gave the download up is dropped: the task
    /// it names is no longer the one the state holds.
    fn finish_payload(
        &mut self,
        task: DownloadId,
        result: Result<Vec<u8>, UpdaterError>,
        cx: &mut Context<Self>,
    ) {
        // Reading the payload is what deleted its task, whichever way the read went:
        // this attempt has nothing left to resume.
        if self.download_task == Some(task) {
            self.download_task = None;
        }
        match result {
            Ok(bytes) => {
                if self.state.download_finished(task, Bytes::from(bytes)) {
                    cx.notify();
                }
            }
            Err(error) => {
                if self
                    .state
                    .download()
                    .is_some_and(|download| download.task == task)
                {
                    self.state.attempt_failed(error);
                    cx.notify();
                } else {
                    log::debug!("the read of an update download that was given up failed: {error}");
                }
            }
        }
    }

    /// Stops a download in flight at its partial file, so it can be resumed.
    pub(crate) fn pause_download(&mut self, cx: &mut Context<Self>) {
        let UpdaterState::Downloading(download) = &self.state else {
            return;
        };
        let task = download.task;
        if self
            .provider
            .update(cx, |provider, cx| provider.pause(task, cx))
            && self.state.pause_started()
        {
            cx.notify();
        }
    }

    /// Continues a stopped download from its partial file.
    pub(crate) fn resume_download(&mut self, cx: &mut Context<Self>) {
        let UpdaterState::Paused(download) = &self.state else {
            return;
        };
        let task = download.task;
        if self
            .provider
            .update(cx, |provider, cx| provider.resume(task, cx))
            && self.state.resume_started()
        {
            cx.notify();
        }
    }

    /// Stops the download in flight and offers the release again.
    pub(crate) fn cancel_download(&mut self, cx: &mut Context<Self>) {
        let Some(download) = self.state.download() else {
            return;
        };
        let task = download.task;
        // A task the manager no longer holds is already gone, which is what the
        // cancel asks for; only a delete the manager accepted has an outcome to
        // follow, because a failed one would leave its task and its temporary file
        // behind with nobody to remove them. Only this task's own removal is put
        // here: another one being followed is not this cancel's to reset.
        if self
            .provider
            .update(cx, |provider, cx| provider.delete(task, cx))
        {
            self.cleanups.insert(task, 0);
        }
        // The manager removes the task and its file, so a later download adds a new
        // one rather than resuming this.
        self.download_task = None;
        self.state.abandon_attempt();
        cx.notify();
    }

    /// Gives up a downloaded payload: the bytes are dropped and the release is offered again.
    pub(crate) fn discard_download(&mut self, cx: &mut Context<Self>) {
        // The task was deleted once its file was read, so there is nothing here a
        // later download could resume.
        self.download_task = None;
        self.state.abandon_attempt();
        cx.notify();
    }

    /// Installs the downloaded payload.
    pub(crate) fn install(&mut self, cx: &mut Context<Self>) {
        let Some((release, bytes)) = self.state.begin_install() else {
            return;
        };
        cx.notify();

        let language = *cx.global::<LanguageCode>();
        let installing = install::install(&self.runtime, &release, bytes, language);
        let outcome = cx.spawn(async move |updater, cx| {
            let result = match installing.await {
                Ok(result) => result,
                Err(error) => Err(InstallError::Task(error)),
            };
            let _ = updater.update(cx, |updater, cx| updater.finish_install(result, cx));
        });

        self.installing = Some(outcome);
    }

    /// Applies what the install produced.
    ///
    /// On Windows nothing arrives here: the library starts the installer and exits
    /// the process from inside the install call. On macOS and Linux the files are
    /// in place and the application leaves, so what the user starts next is the
    /// version that was just installed rather than the one that replaced it.
    fn finish_install(&mut self, result: Result<(), InstallError>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                cx.notify();
                cx.quit();
            }
            Err(error) => {
                self.state.attempt_failed(UpdaterError::Install(error));
                cx.notify();
            }
        }
    }

    /// The dialog closed on a failure: the release is offered again.
    pub(crate) fn dismiss_failure(&mut self, cx: &mut Context<Self>) {
        self.state.failure_dismissed();
        cx.notify();
    }

    /// Puts the entity into a state a test draws, without a network.
    #[cfg(test)]
    pub(crate) fn set_state_for_test(&mut self, state: UpdaterState, cx: &mut Context<Self>) {
        self.state = state;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        net::{Ipv4Addr, SocketAddr},
        pin::Pin,
        sync::Mutex,
        task::{Context as TaskContext, Poll},
        time::Instant,
    };

    use app_config_provider::manager::ConfigManager;
    use download_provider::global::{
        DownloadProviderDeps, entity as provider_entity, install as install_provider,
    };
    use gpui_kit::{AppContext as _, TestAppContext};
    use hyper::{
        Request, Response, StatusCode,
        body::{Body, Frame, Incoming, SizeHint},
        header::{CONTENT_LENGTH, CONTENT_RANGE},
        server::conn::http1,
        service::service_fn,
    };
    use hyper_util::rt::TokioIo;
    use network::transport::hub::TransportPolicy;
    use tokio::{net::TcpListener, time::Sleep};

    use super::*;
    use crate::release::{PlatformKey, PlatformRelease, ReleaseFormat, ReleaseNotes};

    /// How long a test waits for an asynchronous outcome before failing.
    const WAIT_LIMIT: Duration = Duration::from_secs(10);

    /// How much of a paced payload each frame carries.
    const FRAME: usize = 16 * 1024;

    /// How long a paced server waits between frames.
    const PACE: Duration = Duration::from_millis(25);

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

    fn release(version: &str) -> ReleaseInfo {
        ReleaseInfo {
            key: PlatformKey::current().expect("the tests run on a packaged platform"),
            version: semver::Version::parse(version).unwrap(),
            notes: Some(ReleaseNotes::for_test(&[("en", "what changed")])),
            pub_date: None,
            asset: PlatformRelease {
                url: Url::parse("https://example.invalid/kytrix-setup.exe").unwrap(),
                format: ReleaseFormat::Nsis,
            },
        }
    }

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

    /// The state the test server answers from.
    struct Server {
        payload: Bytes,
        pace: Option<Duration>,
        /// The `Range` header of every request answered, in order.
        ranges: Mutex<Vec<Option<String>>>,
    }

    impl Server {
        /// A server that sends the whole entity as fast as the connection takes it.
        fn whole(payload: Bytes) -> Self {
            Self {
                payload,
                pace: None,
                ranges: Mutex::new(Vec::new()),
            }
        }

        /// A server that sends the whole entity one paced frame at a time.
        fn paced(payload: Bytes) -> Self {
            Self {
                payload,
                pace: Some(PACE),
                ranges: Mutex::new(Vec::new()),
            }
        }

        fn ranges(&self) -> Vec<Option<String>> {
            self.ranges
                .lock()
                .expect("the request log is never poisoned")
                .clone()
        }

        fn answer(&self, request: &Request<Incoming>) -> Response<TestBody> {
            let range = request
                .headers()
                .get("range")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            self.ranges
                .lock()
                .expect("the request log is never poisoned")
                .push(range.clone());

            if request.uri().path() != "/payload" {
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

    /// The offered release with its payload at the test server.
    fn release_from(address: SocketAddr) -> ReleaseInfo {
        let mut release = release("1.2.3");
        release.asset.url = Url::parse(&format!("http://{address}/payload"))
            .expect("the test payload URL is a URL");

        release
    }

    /// A release whose payload address refuses the connection: a download of it
    /// fails without anything listening.
    fn unreachable_release() -> ReleaseInfo {
        let mut release = release("1.2.3");
        release.asset.url =
            Url::parse("https://127.0.0.1:9/kytrix-setup.exe").expect("the test URL is a URL");

        release
    }

    fn failure() -> UpdaterError {
        UpdaterError::Transport(UpdateRequestError::Cancelled)
    }

    /// An event the download manager would publish for `id`.
    fn task_event(id: DownloadId, status: DownloadStatus) -> DownloadEvent {
        DownloadEvent::TaskUpdated(DownloadTask {
            id,
            url: Url::parse("https://example.invalid/kytrix-setup.exe").unwrap(),
            destination: std::env::temp_dir().join("kytrix-update-test.bin"),
            status,
        })
    }

    /// The dependencies one entity needs, without a network.
    ///
    /// The download manager's transport accepts plaintext, because the tests that
    /// drive a real transfer serve it over loopback HTTP; the updater's own
    /// transport — the one a check would use — keeps the default policy.
    fn deps(cx: &mut TestAppContext) -> (UpdaterDeps, ConfigNotification<Config>) {
        let provider = cx.update(|cx| {
            if cx.try_global::<ConfigManager>().is_none() {
                let manager = ConfigManager::in_memory(app_config::settings::Config::default(), cx);
                cx.set_global(manager);
            }
            provider_entity(cx).unwrap_or_else(|| {
                install_provider(
                    cx,
                    DownloadProviderDeps {
                        transport: Arc::new(TransportHub::new(TransportPolicy {
                            allow_plaintext: true,
                            extra_root_certificates: Vec::new(),
                        })),
                        runtime: Runtime::for_test(),
                    },
                )
            })
        });

        (
            UpdaterDeps {
                transport: Arc::new(TransportHub::new(TransportPolicy::default())),
                runtime: Runtime::for_test(),
                provider,
            },
            ConfigNotification::new(Arc::new(Config::default())),
        )
    }

    /// An updater over the real download manager, following its tasks' events.
    fn updater_with_provider(
        cx: &mut TestAppContext,
    ) -> (Entity<Updater>, Entity<DownloadProvider>) {
        let (deps, config) = deps(cx);
        let provider = deps.provider.clone();
        // The transfers report back from the runtime's threads, which the
        // deterministic scheduler would otherwise refuse as non-determinism.
        // This drives real HTTP, so it parks like an integration harness.
        cx.executor().allow_parking();
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        cx.update(|cx| updater.update(cx, |updater, cx| updater.watch_provider(cx)));

        (updater, provider)
    }

    #[test]
    fn the_backoff_should_be_bounded_and_then_give_up() {
        assert_eq!(retry_delay(0), Some(Duration::from_secs(60)));
        assert_eq!(retry_delay(1), Some(Duration::from_secs(5 * 60)));
        assert_eq!(retry_delay(2), Some(Duration::from_secs(15 * 60)));
        assert_eq!(
            retry_delay(3),
            None,
            "a launch retries three times and then waits for the next one"
        );
        assert_eq!(retry_delay(9), None);
    }

    #[test]
    fn a_check_that_finds_nothing_should_leave_nothing_on_offer() {
        let mut state = UpdaterState::Idle;

        assert!(state.begin_check());
        assert!(state.release().is_none(), "a check is not an offer");
        assert!(
            !state.begin_check(),
            "a check already running is not replaced"
        );

        state.check_finished(None);
        assert!(matches!(state, UpdaterState::Idle));
        assert!(state.offered().is_none());
    }

    #[test]
    fn a_check_that_finds_a_release_should_offer_it() {
        let mut state = UpdaterState::Idle;

        state.begin_check();
        state.check_finished(Some(release("1.2.3")));

        let offered = state.offered().expect("the release is on offer");
        assert_eq!(offered.version, semver::Version::new(1, 2, 3));
        assert!(
            !matches!(state, UpdaterState::Downloading(_)),
            "nothing is downloading yet"
        );
    }

    #[test]
    fn progress_should_only_ever_move_forward() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");

        assert!(state.record_progress(task, 0, Some(100)));
        assert!(state.record_progress(task, 40, Some(100)));
        assert!(
            !state.record_progress(task, 40, Some(100)),
            "the same numbers twice are not news"
        );
        assert!(
            !state.record_progress(task, 10, Some(100)),
            "a stale report is ignored"
        );
        assert!(
            !state.record_progress(DownloadId::new(), 60, Some(100)),
            "a report for a task the state does not hold is not news"
        );
        let UpdaterState::Downloading(download) = &state else {
            panic!("a download that started is the state that holds it");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (40, Some(100)),
            "what the bar is drawn from is what was accepted"
        );
    }

    /// A server that answers `200` after a range request restarts the entity, which
    /// the manager reports as a report of zero: that is the new attempt's start, not
    /// a stale number, and the bar has to follow it back down.
    #[test]
    fn progress_should_follow_a_server_that_restarts_the_entity() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");

        assert!(state.record_progress(task, 100, Some(100)));
        assert!(
            state.record_progress(task, 0, Some(100)),
            "a restart at zero is the new attempt starting"
        );
        assert!(state.record_progress(task, 10, Some(100)));
        assert!(state.record_progress(task, 50, Some(100)));
        assert!(
            !state.record_progress(task, 40, Some(100)),
            "a report below the new attempt's progress is still stale"
        );
        let UpdaterState::Downloading(download) = &state else {
            panic!("a download that started is the state that holds it");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (50, Some(100)),
            "the bar follows the attempt that is running"
        );
    }

    #[test]
    fn a_response_that_names_no_length_should_keep_reporting_an_unknown_total() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");

        state.record_progress(task, 0, None);
        state.record_progress(task, 4_096, None);

        let UpdaterState::Downloading(download) = &state else {
            panic!("a download that started is the state that holds it");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (4_096, None),
            "a chunked response has no length to draw a bar against"
        );
    }

    #[test]
    fn only_a_download_in_flight_can_become_ready_to_install() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));

        assert!(
            !state.download_finished(DownloadId::new(), Bytes::from_static(b"payload")),
            "a payload nothing was downloading is not an update to install"
        );
        assert!(matches!(state, UpdaterState::Available(_)));
        assert!(state.begin_install().is_none());
    }

    #[test]
    fn a_download_that_finishes_should_hold_the_payload_until_it_is_installed() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");

        assert!(
            !state.download_finished(DownloadId::new(), Bytes::from_static(b"late")),
            "a payload for a task the state does not hold is not this download's"
        );
        assert!(state.download_finished(task, Bytes::from_static(b"payload")));
        assert!(matches!(state, UpdaterState::ReadyToInstall(_)));
        assert!(state.failure().is_none(), "nothing failed");

        let (installed, bytes) = state
            .begin_install()
            .expect("a ready payload is installable");
        assert_eq!(installed.version, semver::Version::new(1, 2, 3));
        assert_eq!(bytes, Bytes::from_static(b"payload"));
        assert!(matches!(state, UpdaterState::Installing(_)));
        assert!(
            state.begin_install().is_none(),
            "the same payload is not handed to a second installer"
        );
    }

    /// A pause and a completed transfer can race: whichever report arrives last,
    /// the file is complete and the payload is what the user is waiting to install.
    #[test]
    fn a_payload_that_arrives_while_paused_should_still_be_ready_to_install() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");
        state.pause_started();
        state.pause_finished(8, Some(8));
        assert!(matches!(state, UpdaterState::Paused(_)));

        assert!(
            !state.download_finished(DownloadId::new(), Bytes::from_static(b"late")),
            "a payload for a task the state does not hold is not this download's"
        );
        assert!(
            state.download_finished(task, Bytes::from_static(b"payload")),
            "a payload that completed while the pause settled is still this download's"
        );
        assert!(matches!(state, UpdaterState::ReadyToInstall(_)));
        assert!(state.begin_install().is_some());
    }

    #[test]
    fn pausing_should_keep_the_progress_and_settle_as_paused() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");
        state.record_progress(task, 40, Some(100));

        assert!(state.pause_started(), "a running download can be paused");
        assert!(
            !state.pause_started(),
            "a pause that already started is not started twice"
        );
        let UpdaterState::Pausing(download) = &state else {
            panic!("the download being stopped is the pausing state");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (40, Some(100)),
            "the pause keeps the progress the download had"
        );
        assert!(
            state.offered().is_none(),
            "a download being stopped is not on offer again"
        );

        assert!(
            state.pause_finished(40, Some(100)),
            "the stopped transfer settles"
        );
        assert!(
            !state.pause_finished(40, Some(100)),
            "a download that already settled is not settled twice"
        );
        let UpdaterState::Paused(download) = &state else {
            panic!("the stopped download is the paused state");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (40, Some(100)),
            "the paused download keeps the progress it stopped at"
        );
    }

    /// The transfer knows how much it actually wrote when it stopped: a frame that
    /// was in flight at the click is in the numbers it publishes with the pause, so
    /// the settled state takes those rather than the ones it had when the pause
    /// started.
    #[test]
    fn a_stopped_transfer_should_settle_on_the_bytes_it_reported() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");
        state.record_progress(task, 40, Some(100));
        state.pause_started();
        let UpdaterState::Pausing(download) = &state else {
            panic!("the download being stopped is the pausing state");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (40, Some(100)),
            "the pausing state still holds what the click saw"
        );

        assert!(state.pause_finished(72, Some(100)));
        let UpdaterState::Paused(download) = &state else {
            panic!("the stopped download is the paused state");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (72, Some(100)),
            "the settled pause counts the bytes the transfer actually wrote"
        );
    }

    #[test]
    fn resuming_a_paused_download_should_continue_it() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");
        state.pause_started();
        state.pause_finished(0, None);

        assert!(state.resume_started(), "a paused download can be resumed");
        assert!(
            !state.resume_started(),
            "a download that is already running is not resumed twice"
        );
        let UpdaterState::Downloading(download) = &state else {
            panic!("the resumed download is the downloading state");
        };
        assert_eq!(
            (download.accepted(), download.named_total()),
            (0, None),
            "resuming continues from the numbers the download had"
        );
    }

    /// A paused download is still an attempt: giving it up puts the release back on
    /// offer rather than leaving the dialog with nothing to do.
    #[test]
    fn abandoning_a_paused_download_should_offer_the_release_again() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        state
            .begin_download(DownloadId::new())
            .expect("an offered release can be downloaded");
        state.pause_started();
        state.pause_finished(0, None);

        state.abandon_attempt();

        assert!(matches!(state, UpdaterState::Available(_)));
        assert!(state.offered().is_some());
    }

    #[test]
    fn cancelling_a_download_should_offer_the_release_again_with_nothing_held() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        let _ = state.begin_download(task);

        state.abandon_attempt();

        assert!(matches!(state, UpdaterState::Available(_)));
        assert!(
            !matches!(state, UpdaterState::Downloading(_)),
            "the partial bytes went with the attempt"
        );
        assert!(
            !state.record_progress(task, 999, Some(999)),
            "a chunk that arrives after the cancellation is not news"
        );
        assert!(
            !state.download_finished(task, Bytes::from_static(b"late")),
            "a payload that arrives after the cancellation is dropped"
        );
        assert!(state.begin_download(DownloadId::new()).is_some());
    }

    #[test]
    fn a_failed_attempt_should_keep_the_release_on_offer() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        state
            .begin_download(DownloadId::new())
            .expect("an offered release can be downloaded");

        state.attempt_failed(failure());

        assert!(matches!(state, UpdaterState::Failed(_)));
        assert!(state.failure().is_some(), "the dialog has something to say");
        assert!(
            state.offered().is_some(),
            "the update is still there to try again"
        );
        assert!(
            state.begin_install().is_none(),
            "a failed attempt never reaches the installer"
        );

        state.failure_dismissed();
        assert!(matches!(state, UpdaterState::Available(_)));
        assert!(state.failure().is_none());
    }

    #[test]
    fn a_failed_install_should_report_the_same_way_a_failed_download_does() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let task = DownloadId::new();
        state
            .begin_download(task)
            .expect("an offered release can be downloaded");
        state.download_finished(task, Bytes::from_static(b"payload"));
        let _ = state.begin_install();

        state.attempt_failed(UpdaterError::Install(InstallError::ExtractPath));

        assert!(matches!(state, UpdaterState::Failed(_)));
        assert!(state.offered().is_some());
        assert!(!matches!(state, UpdaterState::Installing(_)));
    }

    #[test]
    fn a_failure_before_any_release_is_known_should_not_become_a_failed_state() {
        let mut state = UpdaterState::Idle;

        state.attempt_failed(failure());

        assert!(
            matches!(state, UpdaterState::Idle),
            "there is no release to speak about a failure for"
        );
    }

    /// The check the application runs at startup, in the entity that runs it.
    #[gpui_kit::test]
    fn a_check_should_offer_what_it_found_and_stay_silent_when_it_found_nothing(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.finish_check(Ok(Some(release("1.2.3"))), cx)
            });
        });
        assert!(cx.update(|cx| updater.read(cx).state().offered().is_some()));

        cx.update(|cx| {
            updater.update(cx, |updater, cx| updater.finish_check(Ok(None), cx));
        });
        assert!(
            cx.update(|cx| updater.read(cx).state().offered().is_none()),
            "a check that finds nothing leaves no offer behind"
        );
        assert_eq!(cx.update(|cx| updater.read(cx).failed_checks), 0);
    }

    /// A cancellation that arrives once the payload is already held is not the user
    /// giving it up: what they gave up was a download, and there is none left.
    #[gpui_kit::test]
    fn cancelling_after_the_payload_arrived_should_leave_it_ready_to_install(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::ready_for_test(release("1.2.3"), Bytes::from_static(b"payload")),
                    cx,
                );
            });
        });

        cx.update(|cx| updater.update(cx, |updater, cx| updater.cancel_download(cx)));

        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::ReadyToInstall(_))),
            "the payload the user is about to install is not thrown away by a late cancellation"
        );
    }

    #[gpui_kit::test]
    fn a_failed_check_should_be_counted_and_scheduled_until_the_attempts_run_out(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));

        for spent in 1..=4 {
            cx.update(|cx| {
                updater.update(cx, |updater, cx| {
                    updater.finish_check(
                        Err(UpdaterError::Transport(UpdateRequestError::Cancelled)),
                        cx,
                    );
                });
            });

            assert_eq!(cx.update(|cx| updater.read(cx).failed_checks), spent);
            assert!(
                cx.update(|cx| updater.read(cx).state().offered().is_none()),
                "a failed check offers nothing"
            );
            assert_eq!(
                cx.update(|cx| updater.read(cx).check.is_some()),
                retry_delay(spent - 1).is_some(),
                "the check waiting after {spent} failures is the one the schedule has"
            );
        }
    }

    /// The dialog follows the manager's events only for the task it holds.
    #[gpui_kit::test]
    fn download_events_should_move_only_the_task_the_state_holds(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 0, Some(100)),
                    cx,
                );
                updater.download_event(
                    &task_event(
                        task,
                        DownloadStatus::Pausing {
                            downloaded: 40,
                            total: Some(100),
                        },
                    ),
                    cx,
                );
            });
        });
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Pausing(_))),
            "the manager stopping this task is the pause starting here"
        );

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.download_event(
                    &task_event(
                        task,
                        DownloadStatus::Paused {
                            downloaded: 40,
                            total: Some(100),
                        },
                    ),
                    cx,
                );
                updater.download_event(
                    &task_event(
                        DownloadId::new(),
                        DownloadStatus::Failed {
                            failure: DownloadFailure::Cancelled,
                        },
                    ),
                    cx,
                );
            });
        });
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Paused(_))),
            "a task the state does not hold is not part of this download"
        );
    }

    #[gpui_kit::test]
    fn a_failure_the_download_manager_reports_should_fail_the_download(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 4_096, Some(8_192)),
                    cx,
                );
                updater.download_event(
                    &task_event(
                        task,
                        DownloadStatus::Failed {
                            failure: DownloadFailure::Status { status: 404 },
                        },
                    ),
                    cx,
                );
            });
        });

        let key = cx.update(|cx| {
            updater
                .read(cx)
                .state()
                .failure()
                .map(UpdaterError::message_key)
        });
        assert_eq!(
            key,
            Some("error.updater.download"),
            "a failed transfer is the download failure the dialog speaks"
        );
        assert!(cx.update(|cx| updater.read(cx).state().offered().is_some()));
    }

    #[gpui_kit::test]
    fn a_finished_read_should_become_the_payload_to_install(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 8, Some(8)),
                    cx,
                );
                updater.finish_payload(task, Ok(b"payload".to_vec()), cx);
            });
        });

        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::ReadyToInstall(_))),
            "the bytes of the task the state holds are what it installs"
        );
    }

    /// The read runs after the manager has completed the task, so the user can give
    /// the download up while it is still running: its failure is not theirs to see.
    #[gpui_kit::test]
    fn a_read_that_finishes_after_the_download_was_given_up_should_not_fail_it(
        cx: &mut TestAppContext,
    ) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(release("1.2.3")), cx);
                updater.finish_payload(
                    task,
                    Err(UpdaterError::Download(DownloadFailure::Io {
                        kind: std::io::ErrorKind::NotFound,
                    })),
                    cx,
                );
            });
        });

        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Available(_))),
            "a download the user gave up does not come back as a failure"
        );
    }

    /// A pause or resume the manager refuses leaves the state exactly as it was: the
    /// task the dialog is drawn for is the manager's, not an invention of the state.
    #[gpui_kit::test]
    fn pausing_and_resuming_without_a_provider_task_should_leave_the_state_unchanged(
        cx: &mut TestAppContext,
    ) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 40, Some(100)),
                    cx,
                );
                updater.pause_download(cx);
            });
        });
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Downloading(_))),
            "the manager has no task to pause, so the download did not pause"
        );

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::paused_for_test(release("1.2.3"), task, 40, Some(100)),
                    cx,
                );
                updater.resume_download(cx);
            });
        });
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Paused(_))),
            "the manager has no task to resume, so the download stayed paused"
        );
    }

    /// The task a download starts is what a retry has to resume, so the entity holds
    /// it until the attempt is given up.
    #[gpui_kit::test]
    fn a_download_should_hold_the_task_it_added_until_it_is_cancelled(cx: &mut TestAppContext) {
        // A platform that cannot install a payload never downloads one, so there is
        // no task to hold there.
        if !install::supported() {
            return;
        }
        let (deps, config) = deps(cx);
        let provider = deps.provider.clone();
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        cx.executor().allow_parking();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(unreachable_release()), cx);
                assert_eq!(
                    updater.download_task, None,
                    "nothing has been downloaded yet"
                );
                updater.download(cx);
            });
        });

        let task = cx
            .update(|cx| updater.read(cx).download_task)
            .expect("a download holds the task it added");
        let destination = cx
            .update(|cx| provider.read(cx).task(task).map(|task| task.destination))
            .expect("the task the download holds is one the manager has");
        assert_eq!(
            cx.update(|cx| match updater.read(cx).state() {
                UpdaterState::Downloading(download) => Some(download.task),
                _ => None,
            }),
            Some(task),
            "the state and the held task are the same download"
        );

        cx.update(|cx| updater.update(cx, |updater, cx| updater.cancel_download(cx)));

        assert_eq!(
            cx.update(|cx| updater.read(cx).download_task),
            None,
            "a cancelled attempt has nothing left to resume"
        );
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Available(_))),
            "cancelling offers the release again"
        );

        // The delete is the manager's to finish, and it leaves nothing behind.
        wait_until(cx, "the cancelled task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });
        assert!(
            !destination.exists(),
            "a cancelled download leaves no partial file in the temporary directory"
        );
    }

    /// A failed attempt is retried from the task it left behind rather than replaced
    /// by a second transfer with a second temporary file.
    #[gpui_kit::test]
    fn a_failed_download_should_be_retried_from_the_task_it_holds(cx: &mut TestAppContext) {
        // A platform that cannot install a payload never downloads one.
        if !install::supported() {
            return;
        }
        let (deps, config) = deps(cx);
        let provider = deps.provider.clone();
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        cx.executor().allow_parking();
        // A destination with no file name fails its attempt before any request, so
        // the failed task a retry resumes exists without a network.
        let task = cx.update(|cx| {
            provider.update(cx, |provider, cx| {
                provider.add(
                    DownloadRequest {
                        url: Url::parse("https://example.invalid/kytrix-setup.exe")
                            .expect("the test URL is a URL"),
                        destination: PathBuf::new(),
                        target: RouteTarget::Service(ServiceTarget::KytrixService {
                            capability: KytrixServiceCapability::UpdaterDownload,
                        }),
                        class: HttpClientClass::UpdaterDownload,
                        expected_size: None,
                    },
                    cx,
                )
            })
        });
        wait_until(cx, "the download to fail", |cx| {
            matches!(
                cx.update(|cx| provider.read(cx).task(task).map(|task| task.status)),
                Some(DownloadStatus::Failed { .. })
            )
        });

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.download_task = Some(task);
                updater.set_state_for_test(
                    UpdaterState::failed_for_test(release("1.2.3"), failure()),
                    cx,
                );
                // Dismissing a failure only offers the release again: the failed
                // task is still what the retry resumes.
                updater.dismiss_failure(cx);
                assert!(
                    matches!(updater.state, UpdaterState::Available(_)),
                    "dismissing a failure offers the release again"
                );
                updater.download(cx);

                assert_eq!(
                    updater.download_task,
                    Some(task),
                    "the retry resumes the task the failure left rather than adding another"
                );
                let UpdaterState::Downloading(download) = &updater.state else {
                    panic!("a resumed download is a download in flight");
                };
                assert_eq!(download.task, task, "the state follows the resumed task");
            });
        });
    }

    /// A read that produced the payload ends the attempt's task, like every other
    /// way the attempt can end.
    #[gpui_kit::test]
    fn a_finished_payload_should_clear_the_task_it_read(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.download_task = Some(task);
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 8, Some(8)),
                    cx,
                );
                updater.finish_payload(task, Ok(b"payload".to_vec()), cx);
            });
        });

        assert_eq!(
            cx.update(|cx| updater.read(cx).download_task),
            None,
            "the payload's task was deleted once its file was read"
        );
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::ReadyToInstall(_)))
        );
    }

    /// A read that failed still ends the attempt: the task and its file were
    /// deleted whatever the read produced.
    #[gpui_kit::test]
    fn a_failed_read_should_clear_the_task_and_fail_the_attempt(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.download_task = Some(task);
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 8, Some(8)),
                    cx,
                );
                updater.finish_payload(
                    task,
                    Err(UpdaterError::Download(DownloadFailure::Io {
                        kind: std::io::ErrorKind::NotFound,
                    })),
                    cx,
                );
            });
        });

        assert_eq!(
            cx.update(|cx| updater.read(cx).download_task),
            None,
            "a task whose file was deleted cannot be resumed"
        );
        assert!(
            cx.update(|cx| updater.read(cx).state().failure().is_some()),
            "the failed read is what the dialog speaks about"
        );
    }

    /// Giving up a downloaded payload leaves nothing to resume: its task was deleted
    /// when the file was read.
    #[gpui_kit::test]
    fn a_discarded_payload_should_leave_no_task_to_resume(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.download_task = Some(task);
                updater.set_state_for_test(
                    UpdaterState::ready_for_test(release("1.2.3"), Bytes::from_static(b"payload")),
                    cx,
                );
                updater.discard_download(cx);
            });
        });

        assert_eq!(
            cx.update(|cx| updater.read(cx).download_task),
            None,
            "a discarded payload must not come back through a retry"
        );
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Available(_))),
            "discarding offers the release again"
        );
    }

    /// A cancel the manager refuses — the task is already gone — leaves nothing to
    /// follow: there is no delete outcome that could still leak a file.
    #[gpui_kit::test]
    fn a_cancel_without_a_provider_task_should_not_follow_anything(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 40, Some(100)),
                    cx,
                );
                updater.cancel_download(cx);
                assert!(updater.cleanups.is_empty(), "there is no delete to follow");
                assert_eq!(updater.download_task, None);
                assert!(matches!(updater.state, UpdaterState::Available(_)));
            });
        });
    }

    /// The removal the manager publishes is what ends following a cancelled task;
    /// the removal of another task is not that.
    #[gpui_kit::test]
    fn a_removed_cancelled_task_should_stop_being_followed(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.cleanups.insert(task, 1);
                updater.download_event(&DownloadEvent::TaskRemoved(DownloadId::new()), cx);
                assert_eq!(
                    updater.cleanups.get(&task),
                    Some(&1),
                    "another task's removal is not this task's"
                );
                updater.download_event(&DownloadEvent::TaskRemoved(task), cx);
                assert!(
                    !updater.cleanups.contains_key(&task),
                    "the removal is what the follow waits for"
                );
            });
        });
    }

    /// Two removals can be followed at once — the read of a payload can start while
    /// an earlier cancel is still retrying — so one task's removal ends only that
    /// task's follow.
    #[gpui_kit::test]
    fn a_removed_cleanup_should_leave_the_other_cleanup_alone(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let cancelled = DownloadId::new();
        let read = DownloadId::new();

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.cleanups.insert(cancelled, 1);
                updater.cleanups.insert(read, 0);
                updater.download_event(&DownloadEvent::TaskRemoved(cancelled), cx);
                assert!(
                    !updater.cleanups.contains_key(&cancelled),
                    "the removal ends the follow of the task it names"
                );
                assert_eq!(
                    updater.cleanups.get(&read),
                    Some(&0),
                    "another removal being followed is still followed"
                );
                updater.download_event(&DownloadEvent::TaskRemoved(read), cx);
                assert!(updater.cleanups.is_empty());
            });
        });
    }

    /// A cancelled task whose delete failed is asked for once more, and a second
    /// failure is left in the log: the dialog has already moved on, and a retry
    /// loop would only keep watching a task nobody can remove.
    #[gpui_kit::test]
    fn a_failed_cancel_should_be_retried_once_and_then_given_up(cx: &mut TestAppContext) {
        let (deps, config) = deps(cx);
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        let task = DownloadId::new();
        let failed = task_event(
            task,
            DownloadStatus::Failed {
                failure: DownloadFailure::Io {
                    kind: std::io::ErrorKind::PermissionDenied,
                },
            },
        );

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(release("1.2.3")), cx);
                updater.cleanups.insert(task, 0);
                updater.download_event(&failed, cx);
                assert_eq!(
                    updater.cleanups.get(&task),
                    Some(&1),
                    "the first failure of a delete is asked for again"
                );
                updater.download_event(&failed, cx);
                assert!(
                    !updater.cleanups.contains_key(&task),
                    "a second failure is not retried forever"
                );
                assert!(
                    matches!(updater.state, UpdaterState::Available(_)),
                    "a cancelled task's failure is not the dialog's failure"
                );
            });
        });
    }

    /// Reading a payload gives its task up the same way a cancel does: the delete
    /// that removes its file is followed, so one the file system refuses is retried
    /// rather than leaving the task and its temporary file behind.
    #[gpui_kit::test]
    fn a_read_payload_should_follow_the_delete_of_its_task(cx: &mut TestAppContext) {
        // Reading the file reports back from the runtime's threads, which the
        // deterministic scheduler would otherwise refuse as non-determinism.
        cx.executor().allow_parking();
        let (deps, config) = deps(cx);
        let provider = deps.provider.clone();
        let updater = cx.update(|cx| cx.new(|_| Updater::new(deps, config)));
        cx.update(|cx| updater.update(cx, |updater, cx| updater.watch_provider(cx)));
        // A task of no bytes completes without a network; its empty destination is
        // still a file the delete has to remove.
        let destination = std::env::temp_dir()
            .join(format_smolstr!("kytrix-update-read-{}.bin", DownloadId::new()).as_str());
        let task = cx.update(|cx| {
            provider.update(cx, |provider, cx| {
                provider.add(
                    DownloadRequest {
                        url: Url::parse("https://example.invalid/kytrix-setup.exe")
                            .expect("the test URL is a URL"),
                        destination: destination.clone(),
                        target: RouteTarget::Service(ServiceTarget::KytrixService {
                            capability: KytrixServiceCapability::UpdaterDownload,
                        }),
                        class: HttpClientClass::UpdaterDownload,
                        expected_size: Some(0),
                    },
                    cx,
                )
            })
        });
        wait_until(cx, "the empty task to complete", |cx| {
            matches!(
                cx.update(|cx| provider.read(cx).task(task).map(|task| task.status)),
                Some(DownloadStatus::Completed { .. })
            )
        });
        let completed = cx
            .update(|cx| provider.read(cx).task(task))
            .expect("the completed task is the manager's");

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(
                    UpdaterState::downloading_for_test(release("1.2.3"), task, 0, Some(0)),
                    cx,
                );
                updater.download_event(&DownloadEvent::TaskUpdated(completed), cx);
            });
        });
        // One task at a time, so the update the read finishes in is observed before
        // the delete it starts can run.
        let deadline = Instant::now() + WAIT_LIMIT;
        while !cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::ReadyToInstall(_))) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the payload to be read"
            );
            if !cx.executor().tick() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(
            cx.update(|cx| updater.read(cx).cleanups.contains_key(&task)),
            "reading the payload follows the delete that removes its task"
        );

        wait_until(cx, "the manager's task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });
        assert!(
            cx.update(|cx| !updater.read(cx).cleanups.contains_key(&task)),
            "the removal ends the follow"
        );
        assert!(!destination.exists(), "the file the task owned is removed");
    }

    /// The whole download path over a real server and the real manager: the task the
    /// download adds is followed to its payload, and reading that payload removes
    /// both the task and its temporary file.
    #[gpui_kit::test]
    fn a_download_should_reach_ready_to_install_through_the_manager(cx: &mut TestAppContext) {
        // Only a platform that can install a payload downloads one.
        if !install::supported() {
            return;
        }
        let payload = payload(128 * 1024);
        let address = spawn_server(Arc::new(Server::whole(payload.clone())));
        let (updater, provider) = updater_with_provider(cx);

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(release_from(address)), cx);
                updater.download(cx);
            });
        });
        let task = cx
            .update(|cx| updater.read(cx).download_task)
            .expect("the download added a task to the manager");
        let destination = cx
            .update(|cx| provider.read(cx).task(task).map(|task| task.destination))
            .expect("the task the download added is the manager's");

        wait_until(cx, "the payload to be ready to install", |cx| {
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::ReadyToInstall(_)))
        });
        let read = cx
            .update(|cx| match updater.read(cx).state() {
                UpdaterState::ReadyToInstall(update) => Some(update.bytes.clone()),
                _ => None,
            })
            .expect("the state that was waited for is the one read");
        assert_eq!(read, payload, "the payload is what the server sent");

        wait_until(cx, "the manager's task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });
        assert!(
            !destination.exists(),
            "reading the payload removed the file the task owned"
        );
        let mut part = destination.into_os_string();
        part.push(".part");
        assert!(
            !PathBuf::from(part).exists(),
            "the partial file went with the task"
        );
    }

    /// Pausing and resuming over a real server and the real manager: the pause keeps
    /// the partial file, the resume continues it with a range request, and what
    /// arrives is the payload the state offers to install.
    #[gpui_kit::test]
    fn a_paused_download_should_resume_through_the_manager(cx: &mut TestAppContext) {
        // Only a platform that can install a payload downloads one.
        if !install::supported() {
            return;
        }
        let payload = payload(192 * 1024);
        let server = Arc::new(Server::paced(payload.clone()));
        let address = spawn_server(Arc::clone(&server));
        let (updater, provider) = updater_with_provider(cx);

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(release_from(address)), cx);
                updater.download(cx);
            });
        });
        wait_until(cx, "some bytes to arrive before the pause", |cx| {
            cx.update(|cx| {
                matches!(
                    updater.read(cx).state(),
                    UpdaterState::Downloading(download) if download.accepted() > 0
                )
            })
        });

        cx.update(|cx| updater.update(cx, |updater, cx| updater.pause_download(cx)));
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Pausing(_))),
            "a pause the manager accepted is the state the dialog draws"
        );
        wait_until(cx, "the download to settle as paused", |cx| {
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Paused(_)))
        });

        cx.update(|cx| updater.update(cx, |updater, cx| updater.resume_download(cx)));
        wait_until(cx, "the resumed download to be ready to install", |cx| {
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::ReadyToInstall(_)))
        });
        let read = cx
            .update(|cx| match updater.read(cx).state() {
                UpdaterState::ReadyToInstall(update) => Some(update.bytes.clone()),
                _ => None,
            })
            .expect("the state that was waited for is the one read");
        assert_eq!(
            read, payload,
            "the resumed transfer assembled the whole payload"
        );

        wait_until(cx, "the manager's task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });
        let ranges = server.ranges();
        assert_eq!(
            ranges.len(),
            2,
            "the pause and the resume make two requests: {ranges:?}"
        );
        assert_eq!(
            ranges[0], None,
            "the first request asks for the whole entity"
        );
        let resumed = ranges[1]
            .as_deref()
            .expect("the resume asks for the bytes it already has");
        assert!(
            resumed.starts_with("bytes=") && resumed.ends_with('-'),
            "the resume continues from the partial file: {resumed}"
        );
    }

    /// Cancelling over a real server and the real manager removes the task and its
    /// temporary file, and a download started afterwards gets a task of its own
    /// rather than resuming the one that was cancelled.
    #[gpui_kit::test]
    fn a_cancelled_download_should_be_removed_and_not_resumed(cx: &mut TestAppContext) {
        // Only a platform that can install a payload downloads one.
        if !install::supported() {
            return;
        }
        let address = spawn_server(Arc::new(Server::paced(payload(192 * 1024))));
        let (updater, provider) = updater_with_provider(cx);

        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.set_state_for_test(UpdaterState::Available(release_from(address)), cx);
                updater.download(cx);
            });
        });
        wait_until(cx, "some bytes to arrive before the cancel", |cx| {
            cx.update(|cx| {
                matches!(
                    updater.read(cx).state(),
                    UpdaterState::Downloading(download) if download.accepted() > 0
                )
            })
        });
        let cancelled = cx
            .update(|cx| updater.read(cx).download_task)
            .expect("the download holds the task it added");
        let destination = cx
            .update(|cx| {
                provider
                    .read(cx)
                    .task(cancelled)
                    .map(|task| task.destination)
            })
            .expect("the task the download added is the manager's");

        cx.update(|cx| updater.update(cx, |updater, cx| updater.cancel_download(cx)));
        assert!(
            cx.update(|cx| updater.read(cx).cleanups.contains_key(&cancelled)),
            "a delete the manager accepted is followed to its outcome"
        );
        assert_eq!(
            cx.update(|cx| updater.read(cx).download_task),
            None,
            "a cancelled attempt has nothing left to resume"
        );
        assert!(
            cx.update(|cx| matches!(updater.read(cx).state(), UpdaterState::Available(_))),
            "cancelling offers the release again"
        );

        wait_until(cx, "the cancelled task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });
        assert!(
            cx.update(|cx| !updater.read(cx).cleanups.contains_key(&cancelled)),
            "the removal is what the follow waits for"
        );
        assert!(
            !destination.exists(),
            "the cancelled download's file is removed with its task"
        );

        cx.update(|cx| updater.update(cx, |updater, cx| updater.download(cx)));
        let next = cx
            .update(|cx| updater.read(cx).download_task)
            .expect("a fresh download adds a task of its own");
        assert_ne!(next, cancelled, "the cancelled task is not resumed");

        cx.update(|cx| updater.update(cx, |updater, cx| updater.cancel_download(cx)));
        wait_until(cx, "the second cancelled task to be removed", |cx| {
            cx.update(|cx| provider.read(cx).tasks().is_empty())
        });
    }
}

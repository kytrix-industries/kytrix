//! The updater entity: what is known about a newer release, and what is being done about it.
//!
//! Every transition lives on [`UpdaterState`] and is pure, so the whole machine —
//! what a check offers, what a download does to it, what cancelling leaves behind —
//! is exercised without a network. The entity around it owns the requests, the
//! schedule they run on, and the notifications the interface repaints from.

use std::{sync::Arc, time::Duration};

use app_config::network::Config;
use app_config_provider::notification::ConfigNotification;
use bytes::Bytes;
use gpui_kit::{Context, Task};
use i18n::language::codes::LanguageCode;
use network::transport::hub::TransportHub;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
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
    /// The payload is downloaded, waiting for the user.
    ReadyToInstall(DownloadedUpdate),
    /// The payload is being installed.
    Installing(ReleaseInfo),
    /// The last attempt failed; the dialog says so, and the release stays on offer.
    Failed(FailedUpdate),
}

/// A download in flight.
#[derive(Debug)]
pub(crate) struct Download {
    release: ReleaseInfo,
    /// Bytes accepted so far; it never decreases.
    downloaded: u64,
    /// The exact payload size, when the response named one.
    total: Option<u64>,
    /// Cancelling it stops the read and discards the bytes.
    cancellation: CancellationToken,
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
    fn begin_download(&mut self, cancellation: CancellationToken) -> Option<ReleaseInfo> {
        let release = self.offered()?.clone();
        *self = Self::Downloading(Download {
            release: release.clone(),
            downloaded: 0,
            total: None,
            cancellation,
        });

        Some(release)
    }

    /// Records what the response reported; `true` when what is drawn moved.
    fn record_progress(&mut self, downloaded: u64, total: Option<u64>) -> bool {
        let Self::Downloading(download) = self else {
            return false;
        };
        let before = (download.downloaded, download.total);
        // The reader reports what it has accepted, so a number below what is
        // already known is a stale report rather than news.
        if downloaded >= download.downloaded {
            download.downloaded = downloaded;
        }
        if total.is_some() {
            download.total = total;
        }

        (download.downloaded, download.total) != before
    }

    /// Applies a payload that arrived.
    ///
    /// Only a download in flight becomes ready to install: a payload that arrives
    /// after the user cancelled, or after the state moved on, is dropped here.
    fn download_finished(&mut self, bytes: Bytes) -> bool {
        let Self::Downloading(download) = self else {
            return false;
        };
        *self = Self::ReadyToInstall(DownloadedUpdate {
            release: download.release.clone(),
            bytes,
        });

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
    /// Cancelling a download, closing the dialog on a downloaded payload, and a
    /// cancelled request reporting back all land here, which is what makes each of
    /// them safe to reach twice.
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
        downloaded: u64,
        total: Option<u64>,
        cancellation: CancellationToken,
    ) -> Self {
        Self::Downloading(Download {
            release,
            downloaded,
            total,
            cancellation,
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
    config: ConfigNotification<Config>,
    runtime: Runtime,
    state: UpdaterState,
    /// The check in flight, or the one waiting for its delay.
    ///
    /// `Some` exactly while a check is scheduled or running, so what it holds is
    /// also what says whether this launch still has an attempt left.
    check: Option<Task<()>>,
    /// The download or install in flight.
    attempt: Option<Attempt>,
    /// Failed checks spent since the application started, which the backoff counts.
    failed_checks: usize,
}

/// The two halves of one attempt: what reports its progress, and what reports its outcome.
///
/// Both are held for as long as the attempt lasts and never read: a GPUI task that
/// is dropped is a task that is cancelled, so the field is what keeps the work
/// running rather than what anything asks about it.
struct Attempt {
    _progress: Option<Task<()>>,
    _outcome: Task<()>,
}

impl Updater {
    /// The updater over the composition root's dependencies.
    pub(crate) fn new(deps: UpdaterDeps, config: ConfigNotification<Config>) -> Self {
        Self {
            transport: deps.transport,
            config,
            runtime: deps.runtime,
            state: UpdaterState::Idle,
            check: None,
            attempt: None,
            failed_checks: 0,
        }
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

    /// Downloads the offered release, reporting progress as it arrives.
    pub(crate) fn download(&mut self, cx: &mut Context<Self>) {
        // A platform that cannot install a payload does not fetch one: it would
        // hold the whole package in memory for a button it does not have.
        if !install::supported() {
            return;
        }
        let cancellation = self.transport.cancellation().child_token();
        let Some(release) = self.state.begin_download(cancellation.clone()) else {
            return;
        };
        cx.notify();

        let transport = Arc::clone(&self.transport);
        let config = self.config.current();
        let runtime = self.runtime.clone();
        let resolving = runtime.clone();
        let (progress_sender, mut progress) = mpsc::unbounded_channel();
        let read = cancellation.clone();
        let downloaded = runtime.run(async move {
            fetch::download(
                &transport,
                &config,
                &resolving,
                &release,
                &read,
                move |bytes, total| {
                    // This is called on the task that is reading, so it only hands
                    // the numbers over; what draws them is the entity, and the
                    // entity is updated on the interface thread.
                    let _ = progress_sender.send((bytes, total));
                },
            )
            .await
        });
        let progress = cx.spawn(async move |updater, cx| {
            while let Some((bytes, total)) = progress.recv().await {
                let _ = updater.update(cx, |updater, cx| updater.record_progress(bytes, total, cx));
            }
        });
        let outcome = cx.spawn(async move |updater, cx| {
            let result = match downloaded.await {
                Ok(result) => result,
                Err(error) => Err(UpdaterError::Transport(UpdateRequestError::Task(error))),
            };
            let _ = updater.update(cx, |updater, cx| {
                updater.finish_download(result, &cancellation, cx);
            });
        });

        self.attempt = Some(Attempt {
            _progress: Some(progress),
            _outcome: outcome,
        });
    }

    /// Records the bytes a download has accepted; `total` is `None` while the
    /// response has not named one.
    fn record_progress(&mut self, downloaded: u64, total: Option<u64>, cx: &mut Context<Self>) {
        if self.state.record_progress(downloaded, total) {
            cx.notify();
        }
    }

    /// Applies what the download produced: bytes, a cancellation, or a failure.
    fn finish_download(
        &mut self,
        result: Result<Bytes, UpdaterError>,
        cancellation: &CancellationToken,
        cx: &mut Context<Self>,
    ) {
        // A cancelled request reports whatever its own stage called the
        // cancellation; the token is what says the user asked for it, and the
        // user's own answer is the one that counts. Nothing reaches the installer
        // through this path either: the bytes are dropped.
        if cancellation.is_cancelled() {
            self.state.abandon_attempt();
        } else {
            match result {
                Ok(bytes) => {
                    self.state.download_finished(bytes);
                }
                Err(error) => self.state.attempt_failed(error),
            }
        }
        cx.notify();
    }

    /// Stops the download in flight and offers the release again.
    pub(crate) fn cancel_download(&mut self, cx: &mut Context<Self>) {
        let UpdaterState::Downloading(download) = &self.state else {
            return;
        };
        download.cancellation.cancel();
        self.state.abandon_attempt();
        cx.notify();
    }

    /// Gives up a downloaded payload: the bytes are dropped and the release is offered again.
    pub(crate) fn discard_download(&mut self, cx: &mut Context<Self>) {
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

        self.attempt = Some(Attempt {
            _progress: None,
            _outcome: outcome,
        });
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
    use gpui_kit::AppContext as _;
    use network::transport::hub::TransportPolicy;

    use super::*;
    use crate::release::{PlatformKey, PlatformRelease, ReleaseFormat, ReleaseNotes};

    fn release(version: &str) -> ReleaseInfo {
        ReleaseInfo {
            key: PlatformKey::current().expect("the tests run on a packaged platform"),
            version: semver::Version::parse(version).unwrap(),
            notes: Some(ReleaseNotes::for_test(&[("en", "what changed")])),
            pub_date: None,
            asset: PlatformRelease {
                url: url::Url::parse("https://example.invalid/kytrix-setup.exe").unwrap(),
                format: ReleaseFormat::Nsis,
            },
        }
    }

    fn failure() -> UpdaterError {
        UpdaterError::Transport(UpdateRequestError::Cancelled)
    }

    /// Every attempt at what one entity's dependencies are, without a network.
    fn deps() -> (UpdaterDeps, ConfigNotification<Config>) {
        (
            UpdaterDeps {
                transport: Arc::new(TransportHub::new(TransportPolicy::default())),
                runtime: Runtime::for_test(),
            },
            ConfigNotification::new(Arc::new(Config::default())),
        )
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
        state
            .begin_download(CancellationToken::new())
            .expect("an offered release can be downloaded");

        assert!(state.record_progress(0, Some(100)));
        assert!(state.record_progress(40, Some(100)));
        assert!(
            !state.record_progress(40, Some(100)),
            "the same numbers twice are not news"
        );
        assert!(
            !state.record_progress(10, Some(100)),
            "a stale report is ignored"
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

    #[test]
    fn a_response_that_names_no_length_should_keep_reporting_an_unknown_total() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        state
            .begin_download(CancellationToken::new())
            .expect("an offered release can be downloaded");

        state.record_progress(0, None);
        state.record_progress(4_096, None);

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
            !state.download_finished(Bytes::from_static(b"payload")),
            "a payload nothing was downloading is not an update to install"
        );
        assert!(matches!(state, UpdaterState::Available(_)));
        assert!(state.begin_install().is_none());
    }

    #[test]
    fn a_download_that_finishes_should_hold_the_payload_until_it_is_installed() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        state
            .begin_download(CancellationToken::new())
            .expect("an offered release can be downloaded");

        assert!(state.download_finished(Bytes::from_static(b"payload")));
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

    #[test]
    fn cancelling_a_download_should_offer_the_release_again_with_nothing_held() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        let cancellation = CancellationToken::new();
        let _ = state.begin_download(cancellation.clone());

        cancellation.cancel();
        state.abandon_attempt();

        assert!(matches!(state, UpdaterState::Available(_)));
        assert!(
            !matches!(state, UpdaterState::Downloading(_)),
            "the partial bytes went with the attempt"
        );
        assert!(
            !state.record_progress(999, Some(999)),
            "a chunk that arrives after the cancellation is not news"
        );
        assert!(
            !state.download_finished(Bytes::from_static(b"late")),
            "a payload that arrives after the cancellation is dropped"
        );
        assert!(state.begin_download(CancellationToken::new()).is_some());
    }

    #[test]
    fn a_failed_attempt_should_keep_the_release_on_offer() {
        let mut state = UpdaterState::Idle;
        state.check_finished(Some(release("1.2.3")));
        state
            .begin_download(CancellationToken::new())
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
        state
            .begin_download(CancellationToken::new())
            .expect("an offered release can be downloaded");
        state.download_finished(Bytes::from_static(b"payload"));
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
        let (deps, config) = deps();
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
        let (deps, config) = deps();
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
        let (deps, config) = deps();
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
}

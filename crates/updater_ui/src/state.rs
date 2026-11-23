use std::time::Duration;

use bytes::Bytes;
use download_provider::task::DownloadId;

use crate::{error::UpdaterError, release::ReleaseInfo};

pub(crate) const FIRST_CHECK_DELAY: Duration = Duration::from_secs(5);

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
];

pub(crate) fn retry_delay(spent: usize) -> Option<Duration> {
    RETRY_DELAYS.get(spent).copied()
}

pub(crate) const CANCEL_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub(crate) enum UpdaterState {
    Idle,
    Checking,
    Available(ReleaseInfo),
    Downloading(Download),
    Pausing(Download),
    Paused(Download),
    ReadyToInstall(DownloadedUpdate),
    Installing(ReleaseInfo),
    Failed(FailedUpdate),
}

#[derive(Clone, Debug)]
pub(crate) struct Download {
    pub release: ReleaseInfo,
    pub task: DownloadId,
    downloaded: u64,
    total: Option<u64>,
}

impl Download {
    pub(crate) fn accepted(&self) -> u64 {
        self.downloaded
    }

    pub(crate) fn named_total(&self) -> Option<u64> {
        self.total
    }
}

#[derive(Debug)]
pub(crate) struct DownloadedUpdate {
    pub release: ReleaseInfo,
    pub bytes: Bytes,
}

#[derive(Debug)]
pub(crate) struct FailedUpdate {
    pub release: ReleaseInfo,
    pub error: UpdaterError,
}

impl UpdaterState {
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

    pub(crate) fn offered(&self) -> Option<&ReleaseInfo> {
        match self {
            Self::Available(release) | Self::Failed(FailedUpdate { release, .. }) => Some(release),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn failure(&self) -> Option<&UpdaterError> {
        match self {
            Self::Failed(FailedUpdate { error, .. }) => Some(error),
            _ => None,
        }
    }

    pub(crate) fn download(&self) -> Option<&Download> {
        match self {
            Self::Downloading(download) | Self::Pausing(download) | Self::Paused(download) => {
                Some(download)
            }
            _ => None,
        }
    }

    pub(crate) fn begin_check(&mut self) -> bool {
        if matches!(self, Self::Checking) {
            return false;
        }
        *self = Self::Checking;

        true
    }

    pub(crate) fn check_finished(&mut self, release: Option<ReleaseInfo>) {
        *self = match release {
            Some(release) => Self::Available(release),
            None => Self::Idle,
        };
    }

    pub(crate) fn begin_download(&mut self, task: DownloadId) -> Option<ReleaseInfo> {
        let release = self.offered()?.clone();
        *self = Self::Downloading(Download {
            release: release.clone(),
            task,
            downloaded: 0,
            total: None,
        });

        Some(release)
    }

    pub(crate) fn record_progress(
        &mut self,
        task: DownloadId,
        downloaded: u64,
        total: Option<u64>,
    ) -> bool {
        let Self::Downloading(download) = self else {
            return false;
        };
        if download.task != task {
            return false;
        }
        let before = (download.downloaded, download.total);
        if downloaded == 0 || downloaded >= download.downloaded {
            download.downloaded = downloaded;
        }
        if total.is_some() {
            download.total = total;
        }

        (download.downloaded, download.total) != before
    }

    pub(crate) fn pause_started(&mut self) -> bool {
        let Self::Downloading(download) = self else {
            return false;
        };
        let download = download.clone();
        *self = Self::Pausing(download);

        true
    }

    pub(crate) fn pause_finished(&mut self, downloaded: u64, total: Option<u64>) -> bool {
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

    pub(crate) fn resume_started(&mut self) -> bool {
        let Self::Paused(download) = self else {
            return false;
        };
        let download = download.clone();
        *self = Self::Downloading(download);

        true
    }

    pub(crate) fn download_finished(&mut self, task: DownloadId, bytes: Bytes) -> bool {
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

    pub(crate) fn begin_install(&mut self) -> Option<(ReleaseInfo, Bytes)> {
        let Self::ReadyToInstall(update) = self else {
            return None;
        };
        let release = update.release.clone();
        let bytes = update.bytes.clone();
        *self = Self::Installing(release.clone());

        Some((release, bytes))
    }

    pub(crate) fn attempt_failed(&mut self, error: UpdaterError) {
        let Some(release) = self.release().cloned() else {
            return;
        };
        *self = Self::Failed(FailedUpdate { release, error });
    }

    pub(crate) fn abandon_attempt(&mut self) {
        let Some(release) = self.release().cloned() else {
            return;
        };
        *self = Self::Available(release);
    }

    pub(crate) fn failure_dismissed(&mut self) {
        let Self::Failed(FailedUpdate { release, .. }) = self else {
            return;
        };
        *self = Self::Available(release.clone());
    }

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

    #[cfg(test)]
    pub(crate) fn ready_for_test(release: ReleaseInfo, bytes: Bytes) -> Self {
        Self::ReadyToInstall(DownloadedUpdate { release, bytes })
    }

    #[cfg(test)]
    pub(crate) fn failed_for_test(release: ReleaseInfo, error: UpdaterError) -> Self {
        Self::Failed(FailedUpdate { release, error })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use download_provider::task::DownloadId;
    use url::Url;

    use super::*;
    use crate::{
        error::{InstallError, UpdateRequestError, UpdaterError},
        release::{PlatformKey, PlatformRelease, ReleaseFormat, ReleaseNotes},
    };

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

    fn failure() -> UpdaterError {
        UpdaterError::Transport(UpdateRequestError::Cancelled)
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
}

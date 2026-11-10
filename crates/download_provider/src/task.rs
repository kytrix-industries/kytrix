//! A download's identity, its observable state, and the events that publish it.

use std::{fmt, path::PathBuf};

use url::Url;
use uuid::Uuid;

use crate::error::DownloadFailure;

/// The identity of one download.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DownloadId(Uuid);

impl DownloadId {
    /// Creates an identity that no other download in this process shares.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The underlying UUID.
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for DownloadId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DownloadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One download as its observers see it.
#[derive(Clone, Debug, PartialEq)]
pub struct DownloadTask {
    pub id: DownloadId,
    pub url: Url,
    pub destination: PathBuf,
    pub status: DownloadStatus,
}

/// What a download is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadStatus {
    /// The transfer is in flight.
    Downloading {
        downloaded: u64,
        total: Option<u64>,
        /// The rate the transfer wrote at over the interval this state was last
        /// published at, in megabytes per second, where a megabyte is 1024 × 1024
        /// bytes.
        ///
        /// It is zero while nothing arrives, so a stalled transfer reports zero
        /// rather than its last speed.
        mb_per_second: f32,
    },
    /// The transfer is being stopped and will settle as [`Self::Paused`].
    Pausing { downloaded: u64, total: Option<u64> },
    /// The transfer is stopped at a partial file, and can be resumed.
    Paused { downloaded: u64, total: Option<u64> },
    /// The task and its files are being removed.
    Deleting,
    /// The destination file is complete.
    Completed { downloaded: u64, total: u64 },
    /// The transfer stopped with a failure.
    ///
    /// The partial file is kept when what it holds is still a resume point; a
    /// partial file that cannot be continued — one that outgrew the entity or
    /// the response budget — is removed.
    Failed { failure: DownloadFailure },
}

/// What changed about a download.
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadEvent {
    /// The task was created or its status changed.
    TaskUpdated(DownloadTask),
    /// The task and its files are gone.
    TaskRemoved(DownloadId),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use url::Url;
    use uuid::Uuid;

    use super::{DownloadEvent, DownloadId, DownloadStatus, DownloadTask};
    use crate::error::DownloadFailure;

    fn url() -> Url {
        Url::parse("https://example.invalid/model.bin").unwrap()
    }

    fn task(status: DownloadStatus) -> DownloadTask {
        DownloadTask {
            id: DownloadId::new(),
            url: url(),
            destination: PathBuf::from("models").join("model.bin"),
            status,
        }
    }

    #[test]
    fn a_download_id_is_unique() {
        let first = DownloadId::new();
        let second = DownloadId::new();

        assert_ne!(first, second);
        assert_ne!(DownloadId::default(), DownloadId::default());
    }

    #[test]
    fn a_download_id_displays_the_uuid_it_carries() {
        let id = DownloadId::new();
        let uuid = id.as_uuid();

        assert_eq!(id.to_string(), uuid.to_string());
        assert_eq!(
            Uuid::parse_str(&id.to_string()).unwrap(),
            uuid,
            "the printed identity is the identity"
        );
    }

    #[test]
    fn a_task_snapshot_clones_with_its_status() {
        let statuses = [
            DownloadStatus::Downloading {
                downloaded: 12,
                total: Some(34),
                mb_per_second: 1.5,
            },
            DownloadStatus::Pausing {
                downloaded: 12,
                total: None,
            },
            DownloadStatus::Paused {
                downloaded: 12,
                total: Some(34),
            },
            DownloadStatus::Deleting,
            DownloadStatus::Completed {
                downloaded: 34,
                total: 34,
            },
            DownloadStatus::Failed {
                failure: DownloadFailure::Cancelled,
            },
        ];
        for status in statuses {
            let task = task(status);
            let snapshot = task.clone();

            assert_eq!(snapshot, task);
            assert_eq!(
                DownloadEvent::TaskUpdated(snapshot),
                DownloadEvent::TaskUpdated(task)
            );
        }
    }
}

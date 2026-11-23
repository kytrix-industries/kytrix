use std::{fmt, path::PathBuf};

use url::Url;
use uuid::Uuid;

use crate::error::DownloadFailure;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DownloadId(Uuid);

impl DownloadId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

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

#[derive(Clone, Debug, PartialEq)]
pub struct DownloadTask {
    pub id: DownloadId,
    pub url: Url,
    pub destination: PathBuf,
    pub status: DownloadStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DownloadStatus {
    Downloading {
        downloaded: u64,
        total: Option<u64>,
        mb_per_second: f32,
    },
    Pausing {
        downloaded: u64,
        total: Option<u64>,
    },
    Paused {
        downloaded: u64,
        total: Option<u64>,
    },
    Deleting,
    Completed {
        downloaded: u64,
        total: u64,
    },
    Failed {
        failure: DownloadFailure,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum DownloadEvent {
    TaskUpdated(DownloadTask),
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

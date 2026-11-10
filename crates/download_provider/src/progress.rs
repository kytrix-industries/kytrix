//! What a transfer in flight has written, as the manager publishes it.

use std::{
    sync::{Mutex, PoisonError},
    time::Duration,
};

/// The size of a megabyte, which is what a published rate is measured in.
const MEGABYTE: f32 = 1024.0 * 1024.0;

/// The progress of one transfer, written by the task reading the response and read
/// by the manager once an interval.
///
/// The lock is only ever held to copy the numbers, so a transfer never waits on a
/// reader.
#[derive(Default)]
pub(crate) struct Progress {
    snapshot: Mutex<Snapshot>,
}

/// What a transfer had written at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    written: u64,
    total: Option<u64>,
    opened: bool,
}

impl Progress {
    /// Records the resume point and the size of the entity, once the partial file
    /// is open and the transfer is about to read its first byte.
    pub(crate) fn opened_at(&self, written: u64, total: Option<u64>) {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        *snapshot = Snapshot {
            written,
            total,
            opened: true,
        };
    }

    /// Records the bytes written so far, keeping the size the response named.
    pub(crate) fn wrote(&self, written: u64) {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        snapshot.written = written;
    }

    /// What the manager publishes from.
    pub(crate) fn snapshot(&self) -> Snapshot {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Snapshot {
    /// The bytes written so far, counted from the first byte of the entity.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// The size of the whole entity, when a response named one.
    pub(crate) fn total(&self) -> Option<u64> {
        self.total
    }

    /// Whether the partial file is open.
    ///
    /// It is announced only once an attempt has opened and truncated the file, so
    /// from then on the bytes on disk are the resume point of that attempt.
    pub(crate) fn opened(&self) -> bool {
        self.opened
    }

    /// The megabytes per second the transfer wrote at, `interval` being how long
    /// ago the reading of `earlier` bytes was taken.
    pub(crate) fn megabytes_per_second_since(&self, earlier: u64, interval: Duration) -> f32 {
        if interval.is_zero() {
            return 0.0;
        }
        let written = self.written.saturating_sub(earlier) as f32;

        written / interval.as_secs_f32() / MEGABYTE
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{MEGABYTE, Progress, Snapshot};

    #[test]
    fn a_progress_should_answer_what_the_transfer_wrote() {
        let progress = Progress::default();
        assert_eq!(
            progress.snapshot(),
            Snapshot::default(),
            "an attempt starts having written nothing and having opened nothing",
        );

        progress.opened_at(1024, Some(4096));
        assert_eq!(
            progress.snapshot(),
            Snapshot {
                written: 1024,
                total: Some(4096),
                opened: true,
            },
            "opening the partial file is the resume point the transfer starts at",
        );

        progress.wrote(2048);
        assert_eq!(
            progress.snapshot(),
            Snapshot {
                written: 2048,
                total: Some(4096),
                opened: true,
            },
            "a byte report keeps the size the response named",
        );
    }

    #[test]
    fn a_rate_should_be_the_bytes_written_over_its_interval() {
        let snapshot = Snapshot {
            written: 3 * 1024 * 1024,
            total: None,
            opened: true,
        };

        assert_eq!(
            snapshot.megabytes_per_second_since(1024 * 1024, Duration::from_secs(1)),
            2.0,
            "the bytes written in the interval are the rate of that interval",
        );
        assert_eq!(
            snapshot.megabytes_per_second_since(3 * 1024 * 1024, Duration::from_secs(1)),
            0.0,
            "nothing written is no rate",
        );
        assert_eq!(
            snapshot.megabytes_per_second_since(4 * 1024 * 1024, Duration::from_secs(1)),
            0.0,
            "a transfer restarted at zero wrote nothing of this interval",
        );
        assert_eq!(
            snapshot.megabytes_per_second_since(0, Duration::ZERO),
            0.0,
            "no interval is no rate",
        );
        assert_eq!(
            snapshot.megabytes_per_second_since(0, Duration::from_millis(500)),
            6.0,
            "a shorter interval of the same bytes is a faster rate",
        );
    }

    #[test]
    fn a_megabyte_should_be_what_a_rate_is_measured_in() {
        let snapshot = Snapshot {
            written: MEGABYTE as u64,
            total: None,
            opened: true,
        };

        assert_eq!(
            snapshot.megabytes_per_second_since(0, Duration::from_secs(1)),
            1.0,
            "1024 × 1024 bytes in a second is one megabyte per second",
        );
    }
}

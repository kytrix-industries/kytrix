//! The arithmetic every polling widget ends up needing.
//!
//! A widget that re-reads its data on a schedule needs the same three things
//! whatever it polls: the wall-clock instant its service publishes a deadline as,
//! the wall-clock millisecond a scheduled instant falls on, and a way to read the
//! time left until it. All three were written once per polling widget before this
//! module existed — up to four copies each — which is how a rounding rule gets
//! fixed in one pane and stays wrong in the others.
//!
//! What belongs here is that arithmetic and nothing else; this is not a home for
//! widget helpers in general.

use std::time::{SystemTime, UNIX_EPOCH};

use smol_str::{SmolStr, format_smolstr};
use tokio::time::Instant;

/// The current Unix millisecond, which is the unit a poll deadline is published in.
///
/// A system clock set before the epoch reads as zero rather than as a negative or a
/// panic: a countdown against it then simply reports that its deadline has passed.
pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| {
            i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The Unix millisecond a scheduled instant falls on, which is what a pane counts
/// down to.
///
/// A poll loop schedules on the Tokio clock and publishes deadlines as wall-clock
/// milliseconds, because a pane has to compare them against a clock it can read.
/// The remaining time is measured on the monotonic clock — which cannot go
/// backwards under a clock adjustment — and added to the wall clock exactly once,
/// here.
pub fn deadline_millis(instant: Instant) -> i64 {
    let remaining = instant.saturating_duration_since(Instant::now());
    unix_millis().saturating_add(i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX))
}

/// The time left as a clock, which never reads lower than it is.
///
/// `598_001` milliseconds is "09:59" and not "09:58": a countdown that rounded down
/// would reach zero while there was still a second left to wait. Hours appear only
/// when there are hours left to show, so a ten-minute interval counts down as
/// "10:00" while a candle interval counts down as "01:01:02" — one rule for both,
/// and no field that is always zero.
///
/// A deadline that has already passed reads as `00:00`: a pane whose schedule ran
/// out is not a pane with a negative wait.
pub fn format_countdown(remaining_millis: i64) -> SmolStr {
    let seconds = remaining_millis.max(0).saturating_add(999) / 1_000;
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours == 0 {
        format_smolstr!("{minutes:02}:{seconds:02}")
    } else {
        format_smolstr!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_countdown_should_never_read_lower_than_the_time_left() {
        assert_eq!(format_countdown(0).as_str(), "00:00");
        assert_eq!(format_countdown(1).as_str(), "00:01");
        assert_eq!(format_countdown(58_001).as_str(), "00:59");
        assert_eq!(format_countdown(60_000).as_str(), "01:00");
        assert_eq!(format_countdown(119_500).as_str(), "02:00");
        assert_eq!(format_countdown(598_001).as_str(), "09:59");
        assert_eq!(format_countdown(600_000).as_str(), "10:00");
    }

    #[test]
    fn a_countdown_should_show_hours_only_when_there_are_hours() {
        assert_eq!(format_countdown(3_599_000).as_str(), "59:59");
        assert_eq!(format_countdown(3_600_000).as_str(), "01:00:00");
        assert_eq!(format_countdown(3_662_000).as_str(), "01:01:02");
        assert_eq!(format_countdown(8 * 60 * 60 * 1_000).as_str(), "08:00:00");
        assert_eq!(format_countdown(90_061_000).as_str(), "25:01:01");
    }

    #[test]
    fn a_countdown_should_read_zero_once_its_deadline_has_passed() {
        assert_eq!(format_countdown(-1).as_str(), "00:00");
        assert_eq!(format_countdown(-1_000).as_str(), "00:00");
        assert_eq!(format_countdown(i64::MIN).as_str(), "00:00");
    }

    #[test]
    fn the_clock_should_read_as_unix_milliseconds() {
        // 2020-01-01, so a clock that is merely plausible passes and the two ways
        // this can be wrong — an epoch of zero, or seconds instead of milliseconds —
        // do not.
        assert!(unix_millis() > 1_577_836_800_000);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deadline_should_be_the_wall_clock_millisecond_its_instant_falls_on() {
        let due_now = deadline_millis(Instant::now());
        let in_a_minute = deadline_millis(Instant::now() + Duration::from_secs(60));

        assert!(
            (due_now - unix_millis()).abs() < 1_000,
            "an instant that is already due is now"
        );
        assert!(
            in_a_minute - due_now >= 59_000,
            "and one a minute away is a minute of wall clock, not a tick count"
        );
        assert_eq!(
            format_countdown(in_a_minute - unix_millis()).as_str(),
            "01:00",
            "which is what a pane then counts down"
        );
    }
}

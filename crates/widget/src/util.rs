use std::time::{SystemTime, UNIX_EPOCH};

use smol_str::{SmolStr, format_smolstr};
use tokio::time::Instant;

pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| {
            i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
        })
}

pub fn deadline_millis(instant: Instant) -> i64 {
    let remaining = instant.saturating_duration_since(Instant::now());
    unix_millis().saturating_add(i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX))
}

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

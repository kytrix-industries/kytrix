//! The clock the settings say data timestamps are read in.
//!
//! A widget that draws a timestamp asks for the offset on the frame it draws it: the
//! setting can move while the widget is open, and the frame that follows the change
//! is what reads the new value. Reading it here rather than formatting once when
//! data arrives is what keeps a converted clock from being cached into a snapshot.
//!
//! The offset comes from the settings snapshot the composition root publishes as a
//! signal ([`WidgetServices::config`](crate::services::WidgetServices::config)):
//! a widget that draws a timestamp reads the signal inside its `bind_text` closure —
//! `let config = rcx.read(&services.config); let offset = data_time_zone(&config);`
//! — so moving the setting invalidates exactly the nodes that drew it. The previous
//! version of this module subscribed to a settings event and called `cx.notify()`;
//! the engine has no subscription seam (design §11.7), and the signal read is the
//! whole replacement.
//!
//! A snapshot the settings never wrote reads UTC, which is the setting's default.

use app_config::{settings::Config, time_zone::DataTimeZone};
use time::UtcOffset;

/// The offset every data timestamp is displayed in, read from the settings snapshot.
pub fn data_time_zone(config: &Config) -> UtcOffset {
    utc_offset(config.data_time_zone)
}

/// The offset the setting names.
///
/// Every value the setting holds is one a UTC offset can express, so this cannot
/// fail; the range is enforced where the value is decoded.
fn utc_offset(zone: DataTimeZone) -> UtcOffset {
    UtcOffset::from_hms(zone.hours(), 0, 0)
        .expect("an offset the setting offers is a valid UTC offset")
}

#[cfg(test)]
mod tests {
    use app_config::{settings::Config, time_zone::DataTimeZone};

    use super::{data_time_zone, utc_offset};

    /// Every offset the setting offers is the UTC offset of that many hours, and the
    /// one the default configuration names is UTC.
    #[test]
    fn an_offered_offset_should_be_that_many_hours_from_utc() {
        for zone in DataTimeZone::ALL {
            let offset = utc_offset(zone);

            assert_eq!(offset.whole_hours(), zone.hours());
            assert_eq!(offset.whole_seconds(), i32::from(zone.hours()) * 3_600);
        }

        assert_eq!(
            data_time_zone(&Config::default()),
            time::UtcOffset::UTC,
            "a snapshot no setting moved reads UTC"
        );
    }

    /// The snapshot is the whole input: moving the setting moves the offset the next
    /// frame reads.
    #[test]
    fn moving_the_setting_moves_the_offset_the_snapshot_answers() {
        let mut config = Config::default();
        config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
        assert_eq!(data_time_zone(&config).whole_hours(), 8);

        config.data_time_zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
        assert_eq!(data_time_zone(&config).whole_hours(), -3);
    }
}

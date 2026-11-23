use app_config::{settings::Config, time_zone::DataTimeZone};
use time::UtcOffset;

pub fn data_time_zone(config: &Config) -> UtcOffset {
    utc_offset(config.data_time_zone)
}

fn utc_offset(zone: DataTimeZone) -> UtcOffset {
    UtcOffset::from_hms(zone.hours(), 0, 0)
        .expect("an offset the setting offers is a valid UTC offset")
}

#[cfg(test)]
mod tests {
    use app_config::{settings::Config, time_zone::DataTimeZone};

    use super::{data_time_zone, utc_offset};

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

    #[test]
    fn moving_the_setting_moves_the_offset_the_snapshot_answers() {
        let mut config = Config::default();
        config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is an offered offset");
        assert_eq!(data_time_zone(&config).whole_hours(), 8);

        config.data_time_zone = DataTimeZone::from_hours(-3).expect("UTC-3 is an offered offset");
        assert_eq!(data_time_zone(&config).whole_hours(), -3);
    }
}

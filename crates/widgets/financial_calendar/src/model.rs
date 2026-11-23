use std::sync::Arc;

use kytrix_proto::types::v1 as proto_types;
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use time::{Date, Month, OffsetDateTime, UtcOffset};

const SECONDS_PER_DAY: u32 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Importance {
    Unspecified,
    Low,
    Medium,
    High,
}

impl Importance {
    fn of(code: i32) -> Self {
        match proto_types::FinancialCalendarEventImportance::try_from(code) {
            Ok(proto_types::FinancialCalendarEventImportance::Low) => Self::Low,
            Ok(proto_types::FinancialCalendarEventImportance::Medium) => Self::Medium,
            Ok(proto_types::FinancialCalendarEventImportance::High) => Self::High,
            Ok(proto_types::FinancialCalendarEventImportance::Unspecified) | Err(_) => {
                Self::Unspecified
            }
        }
    }

    pub(crate) const fn marks(self) -> usize {
        match self {
            Self::Unspecified => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }

    pub(crate) const LEVELS: [Self; 4] = [Self::Low, Self::Medium, Self::High, Self::Unspecified];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CalendarEvent {
    pub at: i64,
    pub country: SmolStr,
    pub name: SmolStr,
    pub reference: Option<SmolStr>,
    pub importance: Importance,
    pub all_day: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CalendarSnapshot {
    events: Arc<[CalendarEvent]>,
}

impl CalendarSnapshot {
    pub(crate) fn parse(
        calendar: proto_types::FinancialCalendar,
    ) -> Result<Self, CalendarParseError> {
        let mut events = Vec::with_capacity(calendar.events.len());
        for event in calendar.events {
            events.push(CalendarEvent::parse(event)?);
        }
        events.sort_by(|one, other| {
            one.at
                .cmp(&other.at)
                .then_with(|| one.country.cmp(&other.country))
                .then_with(|| one.name.cmp(&other.name))
        });

        Ok(Self {
            events: events.into(),
        })
    }

    pub(crate) fn events(&self) -> &[CalendarEvent] {
        &self.events
    }
}

impl CalendarEvent {
    fn parse(event: proto_types::FinancialCalendarEvent) -> Result<Self, CalendarParseError> {
        let date = date_of(event.year, event.month, event.day)?;
        let seconds_of_day = match event.seconds_of_day {
            Some(seconds) if seconds < SECONDS_PER_DAY => Some(seconds),
            Some(seconds) => return Err(CalendarParseError::InvalidTimeOfDay { seconds }),
            None => None,
        };
        let at = date.midnight().assume_utc().unix_timestamp() * 1_000
            + i64::from(seconds_of_day.unwrap_or_default()) * 1_000;

        Ok(Self {
            at,
            country: SmolStr::new(event.country_code.trim().to_uppercase()),
            name: SmolStr::new(event.name.trim()),
            reference: event
                .reference
                .map(|reference| SmolStr::new(reference.trim()))
                .filter(|reference| !reference.is_empty()),
            importance: Importance::of(event.importance),
            all_day: seconds_of_day.is_none(),
        })
    }
}

fn date_of(year: u32, month: u32, day: u32) -> Result<Date, CalendarParseError> {
    let invalid = || CalendarParseError::InvalidDate { year, month, day };
    let year = i32::try_from(year).map_err(|_| invalid())?;
    let month = u8::try_from(month).map_err(|_| invalid())?;
    let month = Month::try_from(month).map_err(|_| invalid())?;
    let day = u8::try_from(day).map_err(|_| invalid())?;

    Date::from_calendar_date(year, month, day).map_err(|_| invalid())
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CalendarParseError {
    #[error("the calendar carried a date that is not one: {year}-{month}-{day}")]
    InvalidDate { year: u32, month: u32, day: u32 },
    #[error("the calendar carried a time of day that is not one: {seconds} seconds")]
    InvalidTimeOfDay { seconds: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalendarRow {
    Day(Date),
    Event(usize),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CalendarFilter {
    pub importance: Option<Importance>,
    pub date: Option<Date>,
    pub country: Option<SmolStr>,
}

impl CalendarFilter {
    pub(crate) fn is_all(&self) -> bool {
        self.importance.is_none() && self.date.is_none() && self.country.is_none()
    }

    fn keeps(&self, event: &CalendarEvent, date: Date) -> bool {
        self.importance
            .is_none_or(|importance| event.importance == importance)
            && self.date.is_none_or(|selected| selected == date)
            && self
                .country
                .as_ref()
                .is_none_or(|country| country == &event.country)
    }

    pub(crate) fn retain_offered(&mut self, dates: &[Date], countries: &[SmolStr]) -> bool {
        let mut changed = false;
        if self.date.is_some_and(|date| !dates.contains(&date)) {
            self.date = None;
            changed = true;
        }
        if self
            .country
            .as_ref()
            .is_some_and(|country| !countries.contains(country))
        {
            self.country = None;
            changed = true;
        }

        changed
    }
}

pub(crate) fn rows_of(
    events: &[CalendarEvent],
    zone: UtcOffset,
    filter: &CalendarFilter,
) -> Vec<CalendarRow> {
    let mut dated = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let date = event_date(event, zone);
        if filter.keeps(event, date) {
            dated.push((date, index));
        }
    }
    dated.sort_by(|&(one_date, one_index), &(other_date, other_index)| {
        let one = &events[one_index];
        let other = &events[other_index];

        one_date
            .cmp(&other_date)
            .then_with(|| one.all_day.cmp(&other.all_day))
            .then_with(|| one.at.cmp(&other.at))
    });

    let mut rows = Vec::with_capacity(events.len().saturating_mul(2));
    let mut day = None;
    for (date, index) in dated {
        if day != Some(date) {
            day = Some(date);
            rows.push(CalendarRow::Day(date));
        }
        rows.push(CalendarRow::Event(index));
    }

    rows
}

pub(crate) fn dates_of(events: &[CalendarEvent], zone: UtcOffset) -> Vec<Date> {
    let mut dates = events
        .iter()
        .map(|event| event_date(event, zone))
        .collect::<Vec<_>>();
    dates.sort_unstable();
    dates.dedup();

    dates
}

pub(crate) fn countries_of(events: &[CalendarEvent]) -> Vec<SmolStr> {
    let mut countries = events
        .iter()
        .filter(|event| !event.country.is_empty())
        .map(|event| event.country.clone())
        .collect::<Vec<_>>();
    countries.sort_unstable();
    countries.dedup();

    countries
}

fn event_date(event: &CalendarEvent, zone: UtcOffset) -> Date {
    let date = if event.all_day {
        local_date(event.at, UtcOffset::UTC)
    } else {
        local_date(event.at, zone).or_else(|| local_date(event.at, UtcOffset::UTC))
    };

    date.unwrap_or(Date::MIN)
}

fn local_date(at: i64, zone: UtcOffset) -> Option<Date> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000)
        .ok()?
        .checked_to_offset(zone)
        .map(|datetime| datetime.date())
}

pub(crate) fn format_time(event: &CalendarEvent, zone: UtcOffset) -> SmolStr {
    if event.all_day {
        return SmolStr::default();
    }
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(event.at) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
        .map_or_else(
            || SmolStr::new_static("--:--"),
            |datetime| format_smolstr!("{:02}:{:02}", datetime.hour(), datetime.minute()),
        )
}

#[cfg(test)]
mod tests {
    use time::Month;

    use super::*;

    fn event(at: i64, country: &str) -> CalendarEvent {
        CalendarEvent {
            at,
            country: SmolStr::new(country),
            name: SmolStr::new_static("event"),
            reference: None,
            importance: Importance::Unspecified,
            all_day: false,
        }
    }

    fn all_day_event(at: i64) -> CalendarEvent {
        CalendarEvent {
            all_day: true,
            ..event(at, "US")
        }
    }

    fn utc(month: Month, day: u8, hour: u8) -> i64 {
        utc_in(2026, month, day, hour)
    }

    fn utc_in(year: i32, month: Month, day: u8, hour: u8) -> i64 {
        Date::from_calendar_date(year, month, day)
            .expect("a real date")
            .with_hms(hour, 0, 0)
            .expect("a real hour")
            .assume_utc()
            .unix_timestamp()
            * 1_000
    }

    fn zone(hours: i8) -> UtcOffset {
        UtcOffset::from_hms(hours, 0, 0).expect("an offered offset")
    }

    fn proto_event(
        at: i64,
        country: &str,
        name: &str,
        importance: i32,
    ) -> proto_types::FinancialCalendarEvent {
        let datetime = OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000)
            .expect("a real instant")
            .to_offset(UtcOffset::UTC);
        let seconds_of_day = u32::from(datetime.hour()) * 3_600
            + u32::from(datetime.minute()) * 60
            + u32::from(datetime.second());

        proto_types::FinancialCalendarEvent {
            year: datetime.year() as u32,
            month: datetime.month() as u32,
            day: datetime.day() as u32,
            seconds_of_day: Some(seconds_of_day),
            country_code: country.to_owned(),
            name: name.to_owned(),
            importance,
            reference: None,
        }
    }

    fn proto_all_day(
        date: Date,
        country: &str,
        name: &str,
        importance: i32,
    ) -> proto_types::FinancialCalendarEvent {
        proto_types::FinancialCalendarEvent {
            year: date.year() as u32,
            month: date.month() as u32,
            day: date.day() as u32,
            seconds_of_day: None,
            country_code: country.to_owned(),
            name: name.to_owned(),
            importance,
            reference: None,
        }
    }

    fn shape(rows: &[CalendarRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                CalendarRow::Day(date) => format!("day {}", date),
                CalendarRow::Event(index) => format!("event {index}"),
            })
            .collect()
    }

    fn rows(events: &[CalendarEvent], zone: UtcOffset) -> Vec<CalendarRow> {
        rows_of(events, zone, &CalendarFilter::default())
    }

    #[test]
    fn an_answer_should_be_ordered_by_the_instant_it_is_read() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![
                proto_event(utc(Month::October, 25, 11), "US", "later", 1),
                proto_event(utc(Month::October, 25, 9), "DE", "earlier", 1),
                proto_event(utc(Month::October, 25, 10), "CN", "middle", 1),
            ],
            timestamp: 9,
        })
        .expect("an answer of three events is readable");

        let names = snapshot
            .events()
            .iter()
            .map(|event| event.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["earlier", "middle", "later"]);
    }

    #[test]
    fn two_events_on_one_instant_should_be_ordered_by_country_and_name() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![
                proto_event(utc(Month::October, 25, 9), "US", "b", 1),
                proto_event(utc(Month::October, 25, 9), "DE", "z", 1),
                proto_event(utc(Month::October, 25, 9), "US", "a", 1),
            ],
            timestamp: 9,
        })
        .expect("an answer of three events is readable");

        let keys = snapshot
            .events()
            .iter()
            .map(|event| (event.country.as_str(), event.name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(keys, [("DE", "z"), ("US", "a"), ("US", "b")]);
    }

    #[test]
    fn every_day_of_an_answer_should_be_headed_by_one_label() {
        let events = [
            event(utc(Month::October, 25, 9), "AU"),
            event(utc(Month::October, 25, 23), "US"),
            event(utc(Month::October, 26, 1), "GB"),
            event(utc(Month::October, 26, 2), "CN"),
        ];

        let rows = rows(&events, UtcOffset::UTC);

        assert_eq!(
            shape(&rows),
            [
                "day 2026-10-25",
                "event 0",
                "event 1",
                "day 2026-10-26",
                "event 2",
                "event 3",
            ]
        );
    }

    #[test]
    fn a_day_should_be_the_day_of_the_zone_the_data_is_read_in() {
        let events = [
            event(utc(Month::October, 25, 9), "AU"),
            event(utc(Month::October, 25, 23), "US"),
        ];

        let rows = rows(&events, zone(8));

        assert_eq!(
            shape(&rows),
            ["day 2026-10-25", "event 0", "day 2026-10-26", "event 1"]
        );
    }

    #[test]
    fn a_day_should_move_backwards_west_of_utc() {
        let events = [
            event(utc(Month::October, 26, 1), "GB"),
            event(utc(Month::October, 26, 12), "US"),
        ];

        let rows = rows(&events, zone(-5));

        assert_eq!(
            shape(&rows),
            ["day 2026-10-25", "event 0", "day 2026-10-26", "event 1"]
        );
    }

    #[test]
    fn an_all_day_event_should_keep_its_date_while_a_timed_event_moves_with_the_zone() {
        let events = [
            all_day_event(utc(Month::October, 25, 0)),
            event(utc(Month::October, 25, 3), "US"),
        ];

        assert_eq!(
            shape(&rows(&events, zone(-5))),
            ["day 2026-10-24", "event 1", "day 2026-10-25", "event 0"],
            "three hours into the UTC day is the previous day five hours west, while \
             the all-day event stays on the date it was given"
        );
        assert_eq!(
            shape(&rows(&events, zone(8))),
            ["day 2026-10-25", "event 1", "event 0"],
            "eight hours east both are that day, and the timed one comes first"
        );
    }

    #[test]
    fn a_day_should_draw_its_timed_events_before_the_untimed_ones() {
        let events = [
            all_day_event(utc(Month::October, 25, 0)),
            event(utc(Month::October, 25, 9), "US"),
            all_day_event(utc(Month::October, 25, 0)),
            event(utc(Month::October, 25, 14), "DE"),
        ];

        assert_eq!(
            shape(&rows(&events, UtcOffset::UTC)),
            ["day 2026-10-25", "event 1", "event 3", "event 0", "event 2",],
            "the clock orders the day, and the two untimed events follow it"
        );
    }

    #[test]
    fn events_that_share_a_time_should_keep_the_order_they_were_read_in() {
        let events = [
            all_day_event(utc(Month::October, 25, 0)),
            event(utc(Month::October, 25, 9), "US"),
            all_day_event(utc(Month::October, 25, 0)),
            event(utc(Month::October, 25, 9), "DE"),
        ];

        assert_eq!(
            shape(&rows(&events, UtcOffset::UTC)),
            ["day 2026-10-25", "event 1", "event 3", "event 0", "event 2",]
        );
    }

    #[test]
    fn an_empty_answer_should_have_no_rows() {
        assert!(rows(&[], zone(8)).is_empty());
    }

    #[test]
    fn a_day_should_be_read_at_every_offered_edge_of_the_setting() {
        let events = [
            event(utc_in(2026, Month::December, 31, 23), "US"),
            event(utc_in(2027, Month::January, 1, 1), "GB"),
        ];

        assert_eq!(
            shape(&rows(&events, UtcOffset::UTC)),
            ["day 2026-12-31", "event 0", "day 2027-01-01", "event 1",],
            "the two instants are the two days either side of the new year"
        );
        assert_eq!(
            shape(&rows(&events, zone(-12))),
            ["day 2026-12-31", "event 0", "event 1"],
            "twelve hours west both instants are still the last day of the old year"
        );
        assert_eq!(
            shape(&rows(&events, zone(14))),
            ["day 2027-01-01", "event 0", "event 1"],
            "fourteen hours east both are already the first day of the new one"
        );
    }

    #[test]
    fn the_time_of_a_day_should_be_read_in_the_zone_the_data_is_shown_in() {
        let at = utc(Month::October, 25, 13);
        let event = event(at, "US");

        assert_eq!(format_time(&event, UtcOffset::UTC).as_str(), "13:00");
        assert_eq!(format_time(&event, zone(8)).as_str(), "21:00");
        assert_eq!(format_time(&event, zone(-5)).as_str(), "08:00");
    }

    #[test]
    fn an_all_day_event_should_have_no_clock_in_any_zone() {
        let all_day = all_day_event(utc(Month::October, 25, 0));

        for zone in [UtcOffset::UTC, zone(8), zone(-5)] {
            assert_eq!(format_time(&all_day, zone).as_str(), "");
        }
    }

    #[test]
    fn an_instant_the_calendar_does_not_hold_should_be_a_timed_out_cell() {
        assert_eq!(
            format_time(&event(i64::MIN, "US"), UtcOffset::UTC).as_str(),
            "--:--"
        );
    }

    #[test]
    fn a_date_the_calendar_does_not_hold_should_be_refused() {
        let mut event = proto_all_day(day(25), "US", "the end of the calendar", 1);
        event.month = 13;

        let error = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![event],
            timestamp: 9,
        })
        .expect_err("a thirteenth month is not a date to group");

        assert_eq!(
            error,
            CalendarParseError::InvalidDate {
                year: 2026,
                month: 13,
                day: 25,
            }
        );
    }

    #[test]
    fn a_time_of_day_the_calendar_does_not_hold_should_be_refused() {
        let mut event = proto_event(utc(Month::October, 25, 9), "US", "later", 1);
        event.seconds_of_day = Some(86_400);

        let error = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![event],
            timestamp: 9,
        })
        .expect_err("a day is 86400 seconds long, and a clock is inside it");

        assert_eq!(
            error,
            CalendarParseError::InvalidTimeOfDay { seconds: 86_400 }
        );
    }

    #[test]
    fn a_country_should_be_read_upper_cased_and_trimmed() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![proto_event(utc(Month::October, 25, 9), " de ", "Ifo", 1)],
            timestamp: 9,
        })
        .expect("an answer of one event is readable");

        assert_eq!(snapshot.events()[0].country.as_str(), "DE");
    }

    #[test]
    fn an_event_should_be_a_date_only_when_the_page_gave_it_no_time() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![
                proto_all_day(day(25), "US", "holiday", 0),
                proto_event(utc(Month::October, 25, 0), "US", "midnight release", 1),
                proto_event(utc(Month::October, 25, 9), "US", "release", 1),
            ],
            timestamp: 9,
        })
        .expect("an answer of three events is readable");

        assert!(snapshot.events()[0].all_day, "no time of day is a date");
        assert_eq!(
            snapshot.events()[0].at,
            utc(Month::October, 25, 0),
            "a date is the midnight that opens it"
        );
        assert!(
            !snapshot.events()[1].all_day,
            "an event at midnight has a clock, and it is zero"
        );
        assert_eq!(
            format_time(&snapshot.events()[1], UtcOffset::UTC).as_str(),
            "00:00"
        );
        assert!(!snapshot.events()[2].all_day);
    }

    #[test]
    fn a_blank_reference_should_be_read_as_no_reference() {
        let reference = |reference: Option<&str>| {
            let mut event = proto_event(1, "US", "GDP", 1);
            event.reference = reference.map(str::to_owned);
            CalendarSnapshot::parse(proto_types::FinancialCalendar {
                events: vec![event],
                timestamp: 9,
            })
            .expect("an answer of one event is readable")
            .events()[0]
                .reference
                .clone()
        };

        assert_eq!(reference(Some("Q3")).as_deref(), Some("Q3"));
        assert_eq!(reference(Some("   ")), None);
        assert_eq!(reference(None), None);
    }

    #[test]
    fn an_importance_should_keep_the_marks_the_page_states() {
        assert_eq!(Importance::of(0).marks(), 0);
        assert_eq!(Importance::of(1).marks(), 1);
        assert_eq!(Importance::of(2).marks(), 2);
        assert_eq!(Importance::of(3).marks(), 3);
        assert_eq!(
            Importance::of(9).marks(),
            0,
            "a code the protocol does not define is not a level this widget can draw"
        );
    }

    #[test]
    fn an_answer_should_keep_every_event_it_carries() {
        let events = (0..6_000_i64)
            .rev()
            .map(|second| proto_event(second * 1_000, "US", "event", 1))
            .collect::<Vec<_>>();

        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events,
            timestamp: 9,
        })
        .expect("a long answer is readable");

        assert_eq!(snapshot.events().len(), 6_000);
        assert_eq!(snapshot.events()[0].at, 0);
        assert_eq!(snapshot.events()[5_999].at, 5_999_000);
    }

    fn day(day: u8) -> Date {
        Date::from_calendar_date(2026, Month::October, day).expect("a day of October 2026")
    }

    #[test]
    fn a_filter_should_be_all_only_when_every_choice_is() {
        assert!(CalendarFilter::default().is_all());
        for filter in [
            CalendarFilter {
                importance: Some(Importance::Low),
                ..CalendarFilter::default()
            },
            CalendarFilter {
                date: Some(day(25)),
                ..CalendarFilter::default()
            },
            CalendarFilter {
                country: Some(SmolStr::new_static("US")),
                ..CalendarFilter::default()
            },
        ] {
            assert!(!filter.is_all(), "{filter:?} narrows the calendar");
        }
    }

    #[test]
    fn an_importance_filter_should_keep_only_its_level() {
        let events = [
            CalendarEvent {
                importance: Importance::High,
                ..event(utc(Month::October, 25, 9), "US")
            },
            CalendarEvent {
                importance: Importance::Low,
                ..event(utc(Month::October, 25, 10), "DE")
            },
            CalendarEvent {
                importance: Importance::High,
                ..event(utc(Month::October, 25, 11), "CN")
            },
        ];
        let filter = CalendarFilter {
            importance: Some(Importance::High),
            ..CalendarFilter::default()
        };

        assert_eq!(
            shape(&rows_of(&events, UtcOffset::UTC, &filter)),
            ["day 2026-10-25", "event 0", "event 2"]
        );
    }

    #[test]
    fn a_country_filter_should_keep_only_its_country() {
        let events = [
            event(utc(Month::October, 25, 9), "US"),
            event(utc(Month::October, 25, 10), "DE"),
            event(utc(Month::October, 25, 11), "US"),
        ];
        let filter = CalendarFilter {
            country: Some(SmolStr::new_static("US")),
            ..CalendarFilter::default()
        };

        assert_eq!(
            shape(&rows_of(&events, UtcOffset::UTC, &filter)),
            ["day 2026-10-25", "event 0", "event 2"]
        );
    }

    #[test]
    fn a_day_filter_should_keep_only_the_events_drawn_under_that_day() {
        let events = [
            event(utc(Month::October, 25, 9), "AU"),
            event(utc(Month::October, 25, 23), "US"),
            event(utc(Month::October, 26, 1), "GB"),
        ];
        let filter = CalendarFilter {
            date: Some(day(26)),
            ..CalendarFilter::default()
        };

        assert_eq!(
            shape(&rows_of(&events, UtcOffset::UTC, &filter)),
            ["day 2026-10-26", "event 2"]
        );
        assert_eq!(
            shape(&rows_of(&events, zone(8), &filter)),
            ["day 2026-10-26", "event 1", "event 2"],
            "eight hours east the late event of the 25th is a row of the 26th"
        );
    }

    #[test]
    fn a_day_with_no_event_left_should_not_be_headed() {
        let events = [
            event(utc(Month::October, 25, 9), "US"),
            event(utc(Month::October, 26, 9), "DE"),
        ];
        let filter = CalendarFilter {
            country: Some(SmolStr::new_static("DE")),
            ..CalendarFilter::default()
        };

        assert_eq!(
            shape(&rows_of(&events, UtcOffset::UTC, &filter)),
            ["day 2026-10-26", "event 1"]
        );
    }

    #[test]
    fn the_days_of_an_answer_should_be_offered_once_each_in_order() {
        let events = [
            event(utc(Month::October, 26, 9), "US"),
            event(utc(Month::October, 25, 9), "DE"),
            event(utc(Month::October, 25, 23), "CN"),
        ];

        assert_eq!(dates_of(&events, UtcOffset::UTC), [day(25), day(26)]);
        assert_eq!(
            dates_of(&events, zone(8)),
            [day(25), day(26)],
            "the two instants of the 25th are still two days eight hours east"
        );
        assert_eq!(
            dates_of(&boundary_events(), zone(8)),
            [day(26)],
            "and both of them are one day there"
        );
    }

    #[test]
    fn the_countries_of_an_answer_should_be_offered_once_each_in_code_order() {
        let events = [
            event(utc(Month::October, 25, 9), "US"),
            event(utc(Month::October, 25, 10), "DE"),
            event(utc(Month::October, 25, 11), "US"),
            event(utc(Month::October, 25, 12), ""),
        ];

        assert_eq!(
            countries_of(&events),
            [SmolStr::new_static("DE"), SmolStr::new_static("US")],
            "a country is offered once, in code order, and an event the page gave no \
             country for is not a country to offer"
        );
    }

    fn boundary_events() -> [CalendarEvent; 2] {
        [
            event(utc(Month::October, 25, 23), "US"),
            event(utc(Month::October, 26, 1), "CN"),
        ]
    }

    #[test]
    fn a_choice_the_new_answer_no_longer_offers_should_fall_back_to_all() {
        let dates = [day(26)];
        let countries = [SmolStr::new_static("DE")];
        let mut filter = CalendarFilter {
            importance: Some(Importance::High),
            date: Some(day(25)),
            country: Some(SmolStr::new_static("US")),
        };

        assert!(
            filter.retain_offered(&dates, &countries),
            "two choices went away, so the header has something to redraw"
        );
        assert_eq!(
            filter,
            CalendarFilter {
                importance: Some(Importance::High),
                date: None,
                country: None,
            },
            "the importance scale is the page's own, so it is not an option the \
             answer can take away"
        );
    }

    #[test]
    fn a_choice_the_new_answer_still_offers_should_be_kept() {
        let dates = [day(25), day(26)];
        let countries = [SmolStr::new_static("US")];
        let filter = CalendarFilter {
            importance: Some(Importance::Low),
            date: Some(day(25)),
            country: Some(SmolStr::new_static("US")),
        };
        let mut kept = filter.clone();

        assert!(!kept.retain_offered(&dates, &countries));
        assert_eq!(kept, filter);
    }

    #[test]
    fn an_empty_answer_should_take_every_choice_with_it() {
        let mut filter = CalendarFilter {
            importance: Some(Importance::High),
            date: Some(day(25)),
            country: Some(SmolStr::new_static("US")),
        };

        assert!(filter.retain_offered(&[], &[]));
        assert_eq!(
            filter,
            CalendarFilter {
                importance: Some(Importance::High),
                date: None,
                country: None,
            }
        );
    }
}

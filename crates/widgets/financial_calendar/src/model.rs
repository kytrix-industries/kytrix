//! What one financial calendar answer is, and how it becomes the rows of a table.
//!
//! The service answers with everything it holds — the whole calendar, every time —
//! so there is nothing to merge or to keep: one answer replaces the last one
//! entirely, and the only work left is layout. Four rules follow from that, and they
//! are the whole of this module:
//!
//! - **the answer is ordered once**, when it is read, so a day never has to wonder
//!   whether the events it holds arrived sorted;
//! - **a day is read in the zone the settings display data in**, so which row starts
//!   a day moves with the setting rather than with the data — the grouping is
//!   recomputed for the new zone, and the text of that row is resolved by the frame
//!   that draws it;
//! - **an all-day event is a date, not an instant**: the protocol writes it as
//!   midnight UTC, and it stays on that UTC day in every zone rather than being
//!   shifted into the previous or next one;
//! - **an instant the calendar cannot hold is refused** rather than drawn: a
//!   timestamp has to be a day in every offset the setting offers, because a row is
//!   grouped by that day.

use std::sync::Arc;

use app_config::time_zone::DataTimeZone;
use gpui_kit::SharedString;
use kytrix_proto::types::v1 as proto_types;
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use time::{Date, OffsetDateTime, UtcOffset};

/// How many milliseconds one day holds.
///
/// The protocol writes an event without a time of day — an all-day event — at
/// midnight UTC of the day it belongs to, so a timestamp on this boundary is a date
/// rather than an instant to move into a display zone.
const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1_000;

/// The furthest west the display setting goes, as an offset.
const WEST: UtcOffset = match UtcOffset::from_hms(DataTimeZone::MIN_HOURS, 0, 0) {
    Ok(offset) => offset,
    // An offset the setting offers is a valid UTC offset by construction, so the
    // fallback is unreachable; it is here because a constant cannot call `expect`.
    Err(_) => UtcOffset::UTC,
};

/// The furthest east the display setting goes, as an offset.
const EAST: UtcOffset = match UtcOffset::from_hms(DataTimeZone::MAX_HOURS, 0, 0) {
    Ok(offset) => offset,
    // See `WEST`.
    Err(_) => UtcOffset::UTC,
};

/// How important an event is.
///
/// The levels are the page's own three styles plus the case where it states none.
/// It is a scale rather than a set of flags: an unknown code on the wire is not a
/// fourth level, it is a statement this widget cannot place, so it is drawn as
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Importance {
    /// The page gave no importance.
    Unspecified,
    Low,
    Medium,
    High,
}

impl Importance {
    /// The level a code on the wire names.
    fn of(code: i32) -> Self {
        match proto_types::FinancialCalendarEventImportance::try_from(code) {
            Ok(proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceLow) => {
                Self::Low
            }
            Ok(
                proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceMedium,
            ) => Self::Medium,
            Ok(proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceHigh) => {
                Self::High
            }
            Ok(
                proto_types::FinancialCalendarEventImportance::FinancialCAlendarEventImportanceUnspecified,
            )
            | Err(_) => Self::Unspecified,
        }
    }

    /// How many marks a row draws for this level: none, one, two or three.
    pub(crate) const fn marks(self) -> usize {
        match self {
            Self::Unspecified => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }
}

/// One event of the calendar.
///
/// The fields are `pub` inside a crate-private type: the drawing side reads them
/// directly, and the whole struct is invisible outside this crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CalendarEvent {
    /// When the event is due, in Unix milliseconds, UTC.
    pub at: i64,
    /// The ISO 3166-1 alpha-2 code of the country it belongs to, upper-cased.
    pub country: SmolStr,
    /// The event's name, as the page states it.
    pub name: SmolStr,
    /// The period the reading is for — a month, a quarter — when the page states
    /// one.
    pub reference: Option<SmolStr>,
    pub importance: Importance,
    /// Whether the page gave the event no time of day.
    ///
    /// Such an event is a date rather than an instant: it belongs to the UTC day the
    /// protocol writes it on, in every zone, and its cell carries no clock.
    pub all_day: bool,
}

/// One complete answer of the calendar service.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CalendarSnapshot {
    /// Every event the answer carried, earliest first.
    events: Arc<[CalendarEvent]>,
}

impl CalendarSnapshot {
    /// Reads one answer of the service.
    pub(crate) fn parse(
        calendar: proto_types::FinancialCalendar,
    ) -> Result<Self, CalendarParseError> {
        let mut events = Vec::with_capacity(calendar.events.len());
        for event in calendar.events {
            events.push(CalendarEvent::parse(event)?);
        }
        // The answer is drawn in the order events happen, so the instant order is made
        // here rather than assumed: two events on the same instant are ordered by
        // their country and then their name, which is what keeps a redraw of the same
        // answer from swapping two rows.
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
    /// Reads one event, refusing an instant the calendar cannot group.
    fn parse(event: proto_types::FinancialCalendarEvent) -> Result<Self, CalendarParseError> {
        // The page writes an event it gives no time of day for at midnight UTC: it is
        // a date, and it is the same date in every zone. Any other instant is a
        // clock, and a row belongs to its day in every offset the setting offers, so
        // an instant without one would move between two groups as the setting moved.
        let all_day = event.timestamp.rem_euclid(MILLIS_PER_DAY) == 0;
        let placeable = if all_day {
            local_date(event.timestamp, UtcOffset::UTC).is_some()
        } else {
            [WEST, EAST]
                .into_iter()
                .all(|zone| local_date(event.timestamp, zone).is_some())
        };
        if !placeable {
            return Err(CalendarParseError::InvalidTimestamp {
                value: event.timestamp,
            });
        }

        Ok(Self {
            at: event.timestamp,
            country: SmolStr::new(event.country_code.trim().to_uppercase()),
            name: SmolStr::new(event.name.trim()),
            reference: event
                .reference
                .map(|reference| SmolStr::new(reference.trim()))
                .filter(|reference| !reference.is_empty()),
            importance: Importance::of(event.importance),
            all_day,
        })
    }
}

/// Why an answer could not be read.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CalendarParseError {
    #[error("the calendar carried an instant that is not a date: {value}")]
    InvalidTimestamp { value: i64 },
}

/// One row of the calendar table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalendarRow {
    /// The label that heads the events of one day.
    Day(Date),
    /// The event at this index of the answer.
    Event(usize),
}

/// The rows an answer is drawn as, with a day label before each day it covers.
///
/// The events of one answer are in order, and a day is a run of them: the label is
/// pushed when the day an event falls on differs from the one before it.
pub(crate) fn rows_of(events: &[CalendarEvent], zone: UtcOffset) -> Vec<CalendarRow> {
    let mut dated = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        // An all-day event is the date the protocol wrote, so the zone does not move
        // it; a timed one belongs to the day its clock reads in the zone. An instant
        // at the very edge of the calendar keeps the day it has in UTC rather than
        // taking the pane down.
        let date = if event.all_day {
            local_date(event.at, UtcOffset::UTC)
        } else {
            local_date(event.at, zone).or_else(|| local_date(event.at, UtcOffset::UTC))
        }
        .unwrap_or(Date::MIN);
        dated.push((date, index));
    }
    // An all-day event keeps its UTC date while a timed one moves with the zone, so
    // the two can put a later instant on an earlier day. Ordering the rows by the day
    // each is drawn under is what keeps a day one run: without it, the label of a day
    // could come back after one that followed it. The sort is stable, so the instant
    // order read above is what orders the events within a day.
    dated.sort_by_key(|&(date, _)| date);

    // Every event of a calendar that spans one day per event is preceded by a label,
    // so twice the events is the most rows there can be and the last reallocation
    // this needs.
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

/// The day an instant falls on in `zone`, when the calendar holds it.
fn local_date(at: i64, zone: UtcOffset) -> Option<Date> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000)
        .ok()?
        .checked_to_offset(zone)
        .map(|datetime| datetime.date())
}

/// The time of day an event is due at, read in the zone the settings display data
/// in, to the minute.
///
/// It is a 24-hour clock in every language: a calendar is read against other
/// clocks — a session's open, another market's close — and a meridiem that is
/// translated is one more thing to convert in the reader's head. The day an event
/// belongs to is the label above it, so the cell carries the time alone.
///
/// An all-day event has no clock to draw: the page gave it a date, and the protocol
/// wrote that date as midnight UTC. Showing midnight — moved into the display zone,
/// no less — would invent a time the page never stated.
pub(crate) fn format_time(event: &CalendarEvent, zone: UtcOffset) -> SharedString {
    if event.all_day {
        return SharedString::default();
    }
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(event.at) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
        .map_or_else(
            || SharedString::new_static("--:--"),
            |datetime| {
                SharedString::new(
                    format_smolstr!("{:02}:{:02}", datetime.hour(), datetime.minute()).as_str(),
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use time::Month;

    use super::*;

    /// An event due at `at`, on `country`, of no importance.
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

    /// The all-day event of the UTC day `at` falls on.
    fn all_day_event(at: i64) -> CalendarEvent {
        CalendarEvent {
            all_day: true,
            ..event(at, "US")
        }
    }

    /// The instant of `day` at `hour` in 2026 UTC.
    fn utc(month: Month, day: u8, hour: u8) -> i64 {
        utc_in(2026, month, day, hour)
    }

    /// The instant of `day` at `hour` of `year` UTC.
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
        proto_types::FinancialCalendarEvent {
            timestamp: at,
            country_code: country.to_owned(),
            name: name.to_owned(),
            importance,
            reference: None,
        }
    }

    /// The labels and events of a grouped answer, as a test reads them.
    fn shape(rows: &[CalendarRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                CalendarRow::Day(date) => format!("day {}", date),
                CalendarRow::Event(index) => format!("event {index}"),
            })
            .collect()
    }

    #[test]
    fn an_answer_should_be_ordered_by_the_instant_it_is_read() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![
                proto_event(3, "US", "later", 1),
                proto_event(1, "DE", "earlier", 1),
                proto_event(2, "CN", "middle", 1),
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

    /// Two events on one instant keep one order between two reads of the same answer:
    /// the sort is what a redraw relies on to draw the rows in the same places.
    #[test]
    fn two_events_on_one_instant_should_be_ordered_by_country_and_name() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![
                proto_event(1, "US", "b", 1),
                proto_event(1, "DE", "z", 1),
                proto_event(1, "US", "a", 1),
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

    /// A day is a run of events, so an answer of three days is three labels and
    /// every event under one of them.
    #[test]
    fn every_day_of_an_answer_should_be_headed_by_one_label() {
        let events = [
            event(utc(Month::October, 25, 9), "AU"),
            event(utc(Month::October, 25, 23), "US"),
            event(utc(Month::October, 26, 1), "GB"),
            event(utc(Month::October, 26, 2), "CN"),
        ];

        let rows = rows_of(&events, UtcOffset::UTC);

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

    /// The setting moves the days, not the events: at UTC+8 an instant late in the
    /// UTC day is the next day, and the label above it says so.
    #[test]
    fn a_day_should_be_the_day_of_the_zone_the_data_is_read_in() {
        let events = [
            event(utc(Month::October, 25, 9), "AU"),
            event(utc(Month::October, 25, 23), "US"),
        ];

        let rows = rows_of(&events, zone(8));

        assert_eq!(
            shape(&rows),
            ["day 2026-10-25", "event 0", "day 2026-10-26", "event 1"]
        );
    }

    /// West of UTC the same instant is the day before, which is the other half of the
    /// same rule.
    #[test]
    fn a_day_should_move_backwards_west_of_utc() {
        let events = [
            event(utc(Month::October, 26, 1), "GB"),
            event(utc(Month::October, 26, 12), "US"),
        ];

        let rows = rows_of(&events, zone(-5));

        assert_eq!(
            shape(&rows),
            ["day 2026-10-25", "event 0", "day 2026-10-26", "event 1"]
        );
    }

    /// An all-day event is a date rather than an instant: the zone does not move it
    /// into another day, while a timed event near UTC midnight can be on the day
    /// before it. The rows follow the day each is drawn under, so a day stays one run
    /// even when the two rules disagree about which day an instant belongs to.
    #[test]
    fn an_all_day_event_should_keep_its_date_while_a_timed_event_moves_with_the_zone() {
        let events = [
            all_day_event(utc(Month::October, 25, 0)),
            event(utc(Month::October, 25, 3), "US"),
        ];

        assert_eq!(
            shape(&rows_of(&events, zone(-5))),
            ["day 2026-10-24", "event 1", "day 2026-10-25", "event 0"],
            "three hours into the UTC day is the previous day five hours west, while \
             the all-day event stays on the date it was given"
        );
        assert_eq!(
            shape(&rows_of(&events, zone(8))),
            ["day 2026-10-25", "event 0", "event 1"],
            "eight hours east both are that day, in the order they were read"
        );
    }

    #[test]
    fn an_empty_answer_should_have_no_rows() {
        assert!(rows_of(&[], zone(8)).is_empty());
    }

    /// The setting's two furthest offsets are the edges of what a day can be: west of
    /// UTC an instant late on the last day of a year is still that day, and east of it
    /// an instant early on the first day of the next one is already that one.
    #[test]
    fn a_day_should_be_read_at_every_offered_edge_of_the_setting() {
        let events = [
            event(utc_in(2026, Month::December, 31, 23), "US"),
            event(utc_in(2027, Month::January, 1, 1), "GB"),
        ];

        assert_eq!(
            shape(&rows_of(&events, UtcOffset::UTC)),
            ["day 2026-12-31", "event 0", "day 2027-01-01", "event 1",],
            "the two instants are the two days either side of the new year"
        );
        assert_eq!(
            shape(&rows_of(&events, zone(-12))),
            ["day 2026-12-31", "event 0", "event 1"],
            "twelve hours west both instants are still the last day of the old year"
        );
        assert_eq!(
            shape(&rows_of(&events, zone(14))),
            ["day 2027-01-01", "event 0", "event 1"],
            "fourteen hours east both are already the first day of the new one"
        );
    }

    #[test]
    fn the_time_of_a_day_should_be_read_in_the_zone_the_data_is_shown_in() {
        let at = utc(Month::October, 25, 13);
        let event = event(at, "US");

        assert_eq!(format_time(&event, UtcOffset::UTC).as_ref(), "13:00");
        assert_eq!(format_time(&event, zone(8)).as_ref(), "21:00");
        assert_eq!(format_time(&event, zone(-5)).as_ref(), "08:00");
    }

    /// An all-day event is a date the page gave, not a clock: midnight UTC is the
    /// protocol's way of writing "no time", and moving it into the display zone would
    /// invent the very time the page left out.
    #[test]
    fn an_all_day_event_should_have_no_clock_in_any_zone() {
        let all_day = all_day_event(utc(Month::October, 25, 0));

        for zone in [UtcOffset::UTC, zone(8), zone(-5)] {
            assert_eq!(format_time(&all_day, zone).as_ref(), "");
        }
    }

    #[test]
    fn an_instant_the_calendar_does_not_hold_should_be_a_timed_out_cell() {
        assert_eq!(
            format_time(&event(i64::MIN, "US"), UtcOffset::UTC).as_ref(),
            "--:--"
        );
    }

    /// An instant the calendar cannot hold is one the grouping could not place, so it
    /// is refused instead of drawn under a label nothing can name.
    #[test]
    fn an_instant_the_calendar_does_not_hold_should_be_refused() {
        let wild = i64::MAX;

        let error = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![proto_event(wild, "US", "the end of the calendar", 1)],
            timestamp: 9,
        })
        .expect_err("an instant with no date is not one to group");

        assert_eq!(error, CalendarParseError::InvalidTimestamp { value: wild });
    }

    #[test]
    fn a_country_should_be_read_upper_cased_and_trimmed() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![proto_event(1, " de ", "Ifo", 1)],
            timestamp: 9,
        })
        .expect("an answer of one event is readable");

        assert_eq!(snapshot.events()[0].country.as_str(), "DE");
    }

    /// Midnight UTC is how the page writes an event without a time of day. A minute
    /// later is a clock, and the two are read differently from then on.
    #[test]
    fn a_midnight_utc_timestamp_should_be_read_as_an_all_day_event() {
        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events: vec![
                proto_event(utc(Month::October, 25, 0), "US", "holiday", 0),
                proto_event(utc(Month::October, 25, 0) + 60_000, "US", "release", 1),
            ],
            timestamp: 9,
        })
        .expect("an answer of two events is readable");

        assert!(snapshot.events()[0].all_day);
        assert!(!snapshot.events()[1].all_day);
    }

    /// A reference the page does not state, or states as blank, is not a period the
    /// row draws: an empty token would leave a gap where one is drawn elsewhere.
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

    /// A whole answer is what the service sent: the table is rebuilt from every event
    /// of it rather than trimmed to a window the widget chose.
    #[test]
    fn an_answer_should_keep_every_event_it_carries() {
        let events = (0..6_000_i64)
            .rev()
            .map(|at| proto_event(at, "US", "event", 1))
            .collect::<Vec<_>>();

        let snapshot = CalendarSnapshot::parse(proto_types::FinancialCalendar {
            events,
            timestamp: 9,
        })
        .expect("a long answer is readable");

        assert_eq!(snapshot.events().len(), 6_000);
        assert_eq!(snapshot.events()[0].at, 0);
        assert_eq!(snapshot.events()[5_999].at, 5_999);
    }
}

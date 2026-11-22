//! The calendar's table model: four columns, a label row above each day, and the
//! choices the header narrows the rows with.
//!
//! The whole answer is the model's only input, so a read that answers again replaces
//! every row rather than patching the ones that moved: a calendar is scheduled ahead,
//! and a revision can add an event in the middle of a day that is already drawn. What
//! the model keeps between reads is nothing but its snapshot, the choices the header
//! narrows it with, and the rows it was grouped into.
//!
//! It is deliberately pure: no nodes, no theme, no toolkit. The view owns the kit
//! table and the cells; this module owns *what* the rows are. Grouping is recomputed
//! whenever the zone the settings display data in moves, which is the one thing
//! outside the data that decides where a day begins. The *text* of each row is
//! resolved by the frame that draws it — the time from the zone showing now, the day
//! label from the language showing now — so moving either is a redraw rather than a
//! new answer.
//!
//! The header's choices are kept here rather than in the view because they decide
//! which rows exist, and because every answer has to be checked against them: the
//! options are the answer's own, so a choice it no longer offers is not a filter any
//! more.

use std::sync::Arc;

use i18n::{t_smol, translate::Translate};
use smol_str::SmolStr;
use time::{Date, Month, UtcOffset, Weekday};

use crate::model::{
    CalendarEvent, CalendarFilter, CalendarRow, CalendarSnapshot, Importance, countries_of,
    dates_of, rows_of,
};

/// The columns, in the order a reader takes a calendar in: how much it matters, when,
/// where, and what.
pub(crate) const COLUMN_COUNT: usize = 4;

/// The column a cell belongs to, named so a draw reads as the column it is.
pub(crate) const IMPORTANCE: usize = 0;
pub(crate) const TIME: usize = 1;
pub(crate) const COUNTRY: usize = 2;
pub(crate) const NAME: usize = 3;

/// The width weight of each column, in header order.
///
/// The name is what a row is read for, so it takes most of the width; the marks, the
/// time and the country are fixed small parts of it. The kit table distributes the
/// remaining width by these weights, which is the engine's spelling of the shares the
/// old dataset declared.
pub(crate) const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.09, 0.14, 0.09, 0.68];

/// The day label's debug selector, which is what says a frame drew the head of a day.
pub(crate) const DAY_SELECTOR: &str = "TEST_FINANCIAL_CALENDAR_DAY";

/// The rows of one answer, narrowed by the choices the header holds.
///
/// A table with no answer yet is the default one: no snapshot, no choice, and no row
/// to draw.
#[derive(Clone, Debug, Default)]
pub(crate) struct CalendarTable {
    snapshot: Arc<CalendarSnapshot>,
    /// The choices the header offers, which decide which of the events are rows.
    filter: CalendarFilter,
    /// The days the answer holds, which is what the header's time control offers.
    dates: Vec<Date>,
    /// The country codes the answer holds, which is what its country control offers.
    countries: Vec<SmolStr>,
    /// The days and events of the answer, in the order they are drawn.
    rows: Arc<[CalendarRow]>,
}

/// Equality is the identity a signal write is deduplicated against.
///
/// The snapshot and the row set are shared pointers: every rebuild mints new ones, so
/// two models are equal exactly when they are the same rebuild of the same answer and
/// no choice moved. That is O(1) — a calendar can carry thousands of events, and this
/// runs on every frame's countdown state.
impl PartialEq for CalendarTable {
    fn eq(&self, other: &Self) -> bool {
        self.filter == other.filter
            && Arc::ptr_eq(&self.snapshot, &other.snapshot)
            && Arc::ptr_eq(&self.rows, &other.rows)
            && self.dates == other.dates
            && self.countries == other.countries
    }
}

impl Eq for CalendarTable {}

impl CalendarTable {
    /// Replaces everything the model holds with one whole answer, grouped for `zone`.
    ///
    /// A read answers with the whole calendar, so the rows are rebuilt rather than
    /// matched against the ones already there: a day may have gained an event, lost
    /// one, or moved to another day as the setting moved.
    pub(crate) fn rebuild(&mut self, snapshot: Arc<CalendarSnapshot>, zone: UtcOffset) {
        self.snapshot = snapshot;
        self.regroup(zone);
    }

    /// Groups the answer the model already holds for another zone.
    ///
    /// The days an answer holds move with the zone as well, so the options the header
    /// offers are rebuilt here too — and a choice that is no longer one of them goes
    /// back to "all".
    pub(crate) fn regroup(&mut self, zone: UtcOffset) {
        self.dates = dates_of(self.snapshot.events(), zone);
        self.countries = countries_of(self.snapshot.events());
        self.filter.retain_offered(&self.dates, &self.countries);
        self.rows = rows_of(self.snapshot.events(), zone, &self.filter).into();
    }

    /// Narrows the rows to one importance level, or to every level.
    pub(crate) fn select_importance(&mut self, importance: Option<Importance>, zone: UtcOffset) {
        self.filter.importance = importance;
        self.regroup(zone);
    }

    /// Narrows the rows to one day, or to every day.
    pub(crate) fn select_date(&mut self, date: Option<Date>, zone: UtcOffset) {
        self.filter.date = date;
        self.regroup(zone);
    }

    /// Narrows the rows to one country, or to every country.
    pub(crate) fn select_country(&mut self, country: Option<SmolStr>, zone: UtcOffset) {
        self.filter.country = country;
        self.regroup(zone);
    }

    /// Puts every choice back to "all".
    pub(crate) fn reset_filter(&mut self, zone: UtcOffset) {
        self.filter = CalendarFilter::default();
        self.regroup(zone);
    }

    /// Drops everything the model draws, which is what a pane that can no longer vouch
    /// for its answer leaves behind.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// The choices the header draws.
    pub(crate) fn filter(&self) -> &CalendarFilter {
        &self.filter
    }

    /// The days the time control offers, earliest first.
    pub(crate) fn dates(&self) -> &[Date] {
        &self.dates
    }

    /// The countries the country control offers, in code order.
    pub(crate) fn countries(&self) -> &[SmolStr] {
        &self.countries
    }

    /// The answer the rows were built from.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> &Arc<CalendarSnapshot> {
        &self.snapshot
    }

    /// The row at `index`, if the table draws one there.
    pub(crate) fn row(&self, index: usize) -> Option<CalendarRow> {
        self.rows.get(index).copied()
    }

    /// The event of the row at `index`, when that row is an event rather than a label.
    pub(crate) fn event(&self, index: usize) -> Option<&CalendarEvent> {
        let CalendarRow::Event(event) = self.row(index)? else {
            return None;
        };
        self.snapshot.events().get(event)
    }

    /// The rows the table draws, in order.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[CalendarRow] {
        &self.rows
    }

    /// The number of rows the table draws.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The span of the cell at `(row, column)`: a day's label heads the events below it
    /// rather than saying anything about one of the columns, so it is one cell across
    /// the whole table.
    pub(crate) fn column_span(&self, row: usize, column: usize) -> usize {
        match self.rows.get(row) {
            Some(CalendarRow::Day(_)) if column == IMPORTANCE => COLUMN_COUNT,
            _ => 1,
        }
    }

    /// The message a table with no row states.
    ///
    /// An answer that carried no event and an answer the header narrowed to nothing are
    /// two different facts, and a reader who chose a filter is owed the second one: "no
    /// data" would read as if the calendar itself were empty.
    pub(crate) fn empty_message_key(&self) -> &'static str {
        if self.filter.is_all() {
            "common.no_data"
        } else {
            "common.no_matches"
        }
    }
}

/// The day a label heads, written the way the language showing writes a date.
///
/// The header's time control offers the same days and draws them with this, so the day
/// a reader picks above the table is the day the table heads its events with.
pub(crate) fn day_label_text(date: Date, translate: &Translate) -> SmolStr {
    let weekday = t_smol!(translate, weekday_key(date.weekday()));
    let month = t_smol!(translate, month_key(date.month()));

    t_smol!(
        translate,
        "widget.financial_calendar.day_label",
        weekday = weekday,
        month = month,
        day = date.day(),
        year = date.year()
    )
}

/// The message key a month is named by.
pub(crate) fn month_key(month: Month) -> &'static str {
    match month {
        Month::January => "widget.financial_calendar.month.january",
        Month::February => "widget.financial_calendar.month.february",
        Month::March => "widget.financial_calendar.month.march",
        Month::April => "widget.financial_calendar.month.april",
        Month::May => "widget.financial_calendar.month.may",
        Month::June => "widget.financial_calendar.month.june",
        Month::July => "widget.financial_calendar.month.july",
        Month::August => "widget.financial_calendar.month.august",
        Month::September => "widget.financial_calendar.month.september",
        Month::October => "widget.financial_calendar.month.october",
        Month::November => "widget.financial_calendar.month.november",
        Month::December => "widget.financial_calendar.month.december",
    }
}

/// The message key a day of the week is named by.
pub(crate) fn weekday_key(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "widget.financial_calendar.weekday.monday",
        Weekday::Tuesday => "widget.financial_calendar.weekday.tuesday",
        Weekday::Wednesday => "widget.financial_calendar.weekday.wednesday",
        Weekday::Thursday => "widget.financial_calendar.weekday.thursday",
        Weekday::Friday => "widget.financial_calendar.weekday.friday",
        Weekday::Saturday => "widget.financial_calendar.weekday.saturday",
        Weekday::Sunday => "widget.financial_calendar.weekday.sunday",
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, rc::Rc};

    use i18n::language::codes::LanguageCode;

    use super::*;
    use crate::test_support::{boundary_snapshot, snapshot};

    /// The translator the product ships, in the language a test asks for.
    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn table(snapshot: Arc<CalendarSnapshot>, zone: UtcOffset) -> CalendarTable {
        let mut table = CalendarTable::default();
        table.rebuild(snapshot, zone);

        table
    }

    /// The day of October 2026 a test filters by.
    fn day(day: u8) -> Date {
        Date::from_calendar_date(2026, Month::October, day).expect("a day of October 2026")
    }

    /// The shape of the rows a table draws, as a test reads it.
    fn shape(table: &CalendarTable) -> Vec<String> {
        table
            .rows()
            .iter()
            .map(|row| match row {
                CalendarRow::Day(date) => format!("day {date}"),
                CalendarRow::Event(index) => format!("event {index}"),
            })
            .collect()
    }

    #[test]
    fn a_table_of_one_day_should_head_it_once() {
        let table = table(snapshot(3), UtcOffset::UTC);

        assert_eq!(
            table.len(),
            4,
            "three events of one UTC day are one label and three rows"
        );
    }

    /// A read answers with the whole calendar, so a second answer of another length is
    /// a table of another length — not the first one with a row patched.
    #[test]
    fn an_answer_should_replace_every_row_it_replaces() {
        let mut table = table(snapshot(3), UtcOffset::UTC);
        assert_eq!(table.len(), 4);

        table.rebuild(snapshot(1), UtcOffset::UTC);

        assert_eq!(table.len(), 2, "one label and the one event left");
    }

    /// The zone is the whole of what regrouping is: the same answer is two days in UTC
    /// and one day eight hours east of it.
    #[test]
    fn a_zone_should_regroup_the_answer_the_table_already_holds() {
        let mut table = table(boundary_snapshot(), UtcOffset::UTC);
        assert_eq!(table.len(), 4, "two labels and two events");

        table.regroup(UtcOffset::from_hms(8, 0, 0).expect("an offered offset"));

        assert_eq!(
            table.len(),
            3,
            "both events are the next day at UTC+8, so one label heads them"
        );
    }

    /// The header's options are the answer's own: a country or a day a reader can
    /// choose is one the calendar holds, and narrowing the rows does not take the
    /// others away.
    #[test]
    fn the_options_a_table_offers_should_come_from_the_whole_answer() {
        let mut table = table(boundary_snapshot(), UtcOffset::UTC);
        assert_eq!(table.dates(), [day(25), day(26)]);
        assert_eq!(
            table.countries(),
            [SmolStr::new_static("CN"), SmolStr::new_static("US")]
        );

        table.select_country(Some(SmolStr::new_static("CN")), UtcOffset::UTC);

        assert_eq!(table.len(), 2, "one label and the one event left");
        assert_eq!(
            table.dates(),
            [day(25), day(26)],
            "the days the answer holds do not depend on the rows drawn"
        );
        assert_eq!(table.countries().len(), 2);
    }

    /// A choice the new answer no longer offers is dropped rather than left filtering
    /// every row away: a table that drew nothing would read as an empty calendar.
    #[test]
    fn a_choice_the_new_answer_no_longer_offers_should_be_dropped() {
        let mut table = table(snapshot(2), UtcOffset::UTC);
        table.select_country(Some(SmolStr::new_static("DE")), UtcOffset::UTC);
        assert_eq!(table.len(), 2, "one label and the one German event");

        table.rebuild(boundary_snapshot(), UtcOffset::UTC);

        assert_eq!(
            table.filter().country,
            None,
            "the new answer holds no German event"
        );
        assert_eq!(
            table.len(),
            4,
            "so every event of the new answer is drawn again"
        );
    }

    /// The days an answer holds move with the zone, so a day the new grouping does not
    /// hold is a choice it cannot keep either.
    #[test]
    fn a_day_the_zone_takes_away_should_be_dropped() {
        let mut table = table(boundary_snapshot(), UtcOffset::UTC);
        table.select_date(Some(day(25)), UtcOffset::UTC);
        assert_eq!(table.len(), 2, "the late event of the 25th alone");

        table.regroup(UtcOffset::from_hms(8, 0, 0).expect("an offered offset"));

        assert_eq!(table.filter().date, None);
        assert_eq!(
            table.len(),
            3,
            "both events are the next day eight hours east"
        );
    }

    /// A pane that can no longer vouch for its answer leaves no choice behind either:
    /// the options went with the data the choice was about.
    #[test]
    fn clearing_the_table_should_clear_its_choices() {
        let mut table = table(boundary_snapshot(), UtcOffset::UTC);
        table.select_country(Some(SmolStr::new_static("US")), UtcOffset::UTC);
        table.select_importance(Some(Importance::High), UtcOffset::UTC);

        table.clear();

        assert!(table.filter().is_all());
        assert_eq!(table.len(), 0);
        assert!(table.dates().is_empty());
        assert!(table.countries().is_empty());
    }

    #[test]
    fn resetting_the_choices_should_draw_every_event_again() {
        let mut table = table(snapshot(3), UtcOffset::UTC);
        table.select_country(Some(SmolStr::new_static("US")), UtcOffset::UTC);
        assert_eq!(table.len(), 3, "one label and the two American events");

        table.reset_filter(UtcOffset::UTC);

        assert!(table.filter().is_all());
        assert_eq!(table.len(), 4);
    }

    /// A table narrowed to nothing says the filter matched nothing; one whose answer
    /// carried nothing says there is no data. The two are different facts.
    #[test]
    fn an_empty_table_should_name_the_reason_it_is_empty() {
        assert_eq!(
            CalendarTable::default().empty_message_key(),
            "common.no_data"
        );
        let mut narrowed = table(snapshot(2), UtcOffset::UTC);
        narrowed.select_importance(Some(Importance::High), UtcOffset::UTC);
        assert_eq!(narrowed.empty_message_key(), "common.no_matches");
    }

    #[test]
    fn every_message_an_empty_table_names_should_be_in_every_locale() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");

        for key in ["common.no_data", "common.no_matches"] {
            for language in i18n.languages() {
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|text| !text.is_empty()),
                    "{key} should be named in {language}"
                );
            }
        }
    }

    #[test]
    fn the_label_of_a_day_should_stand_for_every_column() {
        let table = table(snapshot(1), UtcOffset::UTC);

        assert_eq!(table.column_span(0, IMPORTANCE), COLUMN_COUNT);
        assert_eq!(
            table.column_span(0, TIME),
            1,
            "only the first column is asked"
        );
        assert_eq!(
            table.column_span(1, IMPORTANCE),
            1,
            "an event is a row of its own"
        );
        assert_eq!(
            table.column_span(99, IMPORTANCE),
            1,
            "a row past the table draws nothing to span"
        );
    }

    #[test]
    fn every_month_and_weekday_should_name_itself_once() {
        let months = [
            Month::January,
            Month::February,
            Month::March,
            Month::April,
            Month::May,
            Month::June,
            Month::July,
            Month::August,
            Month::September,
            Month::October,
            Month::November,
            Month::December,
        ]
        .map(month_key);
        let weekdays = [
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
            Weekday::Sunday,
        ]
        .map(weekday_key);
        let keys = months.into_iter().chain(weekdays).collect::<Vec<_>>();
        let distinct = keys.iter().collect::<HashSet<_>>();

        assert_eq!(distinct.len(), keys.len(), "every name is its own key");
        for key in keys {
            assert!(
                key.starts_with("widget.financial_calendar."),
                "`{key}` is not a calendar key"
            );
        }
    }

    /// Every name the day label is built from is one every locale defines.
    ///
    /// The key of a month or a weekday is chosen by number rather than written at a
    /// call site, so nothing else reads it: a key that no locale defines translates
    /// to the empty string, and the label over a day would be missing the very word
    /// that says which day it is.
    #[test]
    fn every_name_a_day_label_is_built_from_should_be_in_every_locale() {
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");
        let keys = [
            Month::January,
            Month::February,
            Month::March,
            Month::April,
            Month::May,
            Month::June,
            Month::July,
            Month::August,
            Month::September,
            Month::October,
            Month::November,
            Month::December,
        ]
        .map(month_key)
        .into_iter()
        .chain(
            [
                Weekday::Monday,
                Weekday::Tuesday,
                Weekday::Wednesday,
                Weekday::Thursday,
                Weekday::Friday,
                Weekday::Saturday,
                Weekday::Sunday,
            ]
            .map(weekday_key),
        )
        .chain(["widget.financial_calendar.day_label"]);

        for language in i18n.languages() {
            for key in keys.clone() {
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|text| !text.is_empty()),
                    "{key} should be named in {language}"
                );
            }
        }
    }

    /// The label above a day is written the way the language showing writes a date,
    /// with the day it belongs to in it: the English one names the weekday first and
    /// the year last, and the Chinese one puts the year first.
    #[test]
    fn a_day_label_should_read_the_way_each_language_writes_a_date() {
        let date = Date::from_calendar_date(2026, Month::October, 25).expect("a real date");

        assert_eq!(
            day_label_text(date, &translate(LanguageCode::En)).as_str(),
            "Sunday, October 25, 2026"
        );
        assert_eq!(
            day_label_text(date, &translate(LanguageCode::ZhCn)).as_str(),
            "2026年10月25日 星期日"
        );
    }

    /// The day a row heads is the day of the zone the table was built for, which is
    /// what the label above the row says.
    #[test]
    fn a_label_should_name_the_day_its_events_fall_on() {
        let table = table(boundary_snapshot(), UtcOffset::UTC);
        let CalendarRow::Day(date) = table.rows()[0] else {
            panic!("the first row of an answer heads a day");
        };

        assert_eq!(
            date,
            Date::from_calendar_date(2026, Month::October, 25).expect("a real date")
        );
    }

    /// Two models built from the same answer are not the same model: a read is a new
    /// row set even when it draws the same calendar, which is what the stale-write
    /// comparison in the view relies on.
    #[test]
    fn a_model_should_only_equal_the_rebuild_it_came_from() {
        let mut first = CalendarTable::default();
        first.rebuild(boundary_snapshot(), UtcOffset::UTC);
        let mut second = CalendarTable::default();
        second.rebuild(first.snapshot().clone(), UtcOffset::UTC);

        assert_ne!(first, second, "a rebuild mints a new row set");
        assert_eq!(
            first, first,
            "the model a signal wrote is the value it reads back"
        );
    }

    /// The rows a table draws are the answer's own order: the shape helper above is the
    /// one thing every view test reads.
    #[test]
    fn an_answer_should_be_drawn_as_the_labels_and_events_it_holds() {
        let table = table(boundary_snapshot(), UtcOffset::UTC);

        assert_eq!(
            shape(&table),
            ["day 2026-10-25", "event 0", "day 2026-10-26", "event 1"]
        );
        assert_eq!(
            table.event(1).map(|event| event.name.as_str()),
            Some("late"),
            "the first event row is the first event of the answer"
        );
        assert_eq!(
            table.event(3).map(|event| event.name.as_str()),
            Some("early")
        );
        assert_eq!(table.event(0), None, "a label is not an event");
    }
}

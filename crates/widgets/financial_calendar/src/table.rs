use std::sync::Arc;

use i18n::{t_smol, translate::Translate};
use smol_str::SmolStr;
use time::{Date, Month, UtcOffset, Weekday};

use crate::model::{
    CalendarEvent, CalendarFilter, CalendarRow, CalendarSnapshot, Importance, countries_of,
    dates_of, rows_of,
};

pub(crate) const COLUMN_COUNT: usize = 4;

pub(crate) const IMPORTANCE: usize = 0;
pub(crate) const TIME: usize = 1;
pub(crate) const COUNTRY: usize = 2;
pub(crate) const NAME: usize = 3;

pub(crate) const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.09, 0.14, 0.09, 0.68];

pub(crate) const DAY_SELECTOR: &str = "TEST_FINANCIAL_CALENDAR_DAY";

#[derive(Clone, Debug, Default)]
pub(crate) struct CalendarTable {
    snapshot: Arc<CalendarSnapshot>,
    filter: CalendarFilter,
    dates: Vec<Date>,
    countries: Vec<SmolStr>,
    rows: Arc<[CalendarRow]>,
}

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
    pub(crate) fn rebuild(&mut self, snapshot: Arc<CalendarSnapshot>, zone: UtcOffset) {
        self.snapshot = snapshot;
        self.regroup(zone);
    }

    pub(crate) fn regroup(&mut self, zone: UtcOffset) {
        self.dates = dates_of(self.snapshot.events(), zone);
        self.countries = countries_of(self.snapshot.events());
        self.filter.retain_offered(&self.dates, &self.countries);
        self.rows = rows_of(self.snapshot.events(), zone, &self.filter).into();
    }

    pub(crate) fn select_importance(&mut self, importance: Option<Importance>, zone: UtcOffset) {
        self.filter.importance = importance;
        self.regroup(zone);
    }

    pub(crate) fn select_date(&mut self, date: Option<Date>, zone: UtcOffset) {
        self.filter.date = date;
        self.regroup(zone);
    }

    pub(crate) fn select_country(&mut self, country: Option<SmolStr>, zone: UtcOffset) {
        self.filter.country = country;
        self.regroup(zone);
    }

    pub(crate) fn reset_filter(&mut self, zone: UtcOffset) {
        self.filter = CalendarFilter::default();
        self.regroup(zone);
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn filter(&self) -> &CalendarFilter {
        &self.filter
    }

    pub(crate) fn dates(&self) -> &[Date] {
        &self.dates
    }

    pub(crate) fn countries(&self) -> &[SmolStr] {
        &self.countries
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> &Arc<CalendarSnapshot> {
        &self.snapshot
    }

    pub(crate) fn row(&self, index: usize) -> Option<CalendarRow> {
        self.rows.get(index).copied()
    }

    pub(crate) fn event(&self, index: usize) -> Option<&CalendarEvent> {
        let CalendarRow::Event(event) = self.row(index)? else {
            return None;
        };
        self.snapshot.events().get(event)
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[CalendarRow] {
        &self.rows
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn column_span(&self, row: usize, column: usize) -> usize {
        match self.rows.get(row) {
            Some(CalendarRow::Day(_)) if column == IMPORTANCE => COLUMN_COUNT,
            _ => 1,
        }
    }

    pub(crate) fn empty_message_key(&self) -> &'static str {
        if self.filter.is_all() {
            "common.no_data"
        } else {
            "common.no_matches"
        }
    }
}

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

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn table(snapshot: Arc<CalendarSnapshot>, zone: UtcOffset) -> CalendarTable {
        let mut table = CalendarTable::default();
        table.rebuild(snapshot, zone);

        table
    }

    fn day(day: u8) -> Date {
        Date::from_calendar_date(2026, Month::October, day).expect("a day of October 2026")
    }

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

    #[test]
    fn an_answer_should_replace_every_row_it_replaces() {
        let mut table = table(snapshot(3), UtcOffset::UTC);
        assert_eq!(table.len(), 4);

        table.rebuild(snapshot(1), UtcOffset::UTC);

        assert_eq!(table.len(), 2, "one label and the one event left");
    }

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
        let i18n = i18n::embedded::load().expect("the embedded locales load");

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

    #[test]
    fn every_name_a_day_label_is_built_from_should_be_in_every_locale() {
        let i18n = i18n::embedded::load().expect("the embedded locales load");
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

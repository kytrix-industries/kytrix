//! The table a calendar is drawn as: four columns, and a label row above each day.
//!
//! The whole answer is the table's only input, so a read that answers again replaces
//! every row rather than patching the ones that moved: a calendar is scheduled ahead,
//! and a revision can add an event in the middle of a day that is already drawn. What
//! the table keeps between reads is nothing but its snapshot, the choices the header
//! narrows it with, and the rows it was grouped into.
//!
//! Grouping is recomputed whenever the zone the settings display data in moves, which
//! is the one thing outside the data that decides where a day begins. The *text* of
//! each row is resolved by the frame that draws it — the time from the zone showing
//! now, the day label from the language showing now — so moving either is a redraw
//! rather than a new answer.
//!
//! The header's choices are kept here rather than in the frame because they decide
//! which rows exist, and because every answer has to be checked against them: the
//! options are the answer's own, so a choice it no longer offers is not a filter any
//! more.

use std::sync::Arc;

use base_ui::{empty_state::EmptyState, style::theme::ThemeExt, table::TableDataSet};
use gpui_kit::{
    AnyElement, App, Empty, FontWeight, InteractiveElement, IntoElement, Length, ParentElement,
    SharedString, Styled, base::Align, div, px, relative,
};
use i18n::t;
use smol_str::SmolStr;
use time::{Date, Month, UtcOffset, Weekday};
use widget::time_zone::data_time_zone;

use crate::model::{
    CalendarEvent, CalendarFilter, CalendarRow, CalendarSnapshot, Importance, countries_of,
    dates_of, format_time, rows_of,
};

/// The columns, in the order a reader takes a calendar in: how much it matters, when,
/// where, and what.
const COLUMN_COUNT: usize = 4;

/// The column a cell belongs to, named so a draw reads as the column it is.
const IMPORTANCE: usize = 0;
const TIME: usize = 1;
const COUNTRY: usize = 2;
const NAME: usize = 3;

/// Column widths in header order.
///
/// The name is what a row is read for, so it takes most of the width; the marks, the
/// time and the country are fixed small parts of it. Every width is a share of the
/// pane, so a narrow pane narrows the name rather than clipping a neighbouring column
/// — the marks take the narrowest share that still holds three dots, and the time room
/// for a five-character clock.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.09)),
    Length::Definite(relative(0.14)),
    Length::Definite(relative(0.09)),
    Length::Definite(relative(0.68)),
];

/// The mark an importance is drawn with.
const MARK_SIZE: f32 = 6.0;

/// The day label's debug selector, which is what says a frame drew the head of a day.
const DAY_SELECTOR: &str = "TEST_FINANCIAL_CALENDAR_DAY";

/// The rows of one answer, narrowed by the choices the header holds.
///
/// A table with no answer yet is the default one: no snapshot, no choice, and no row
/// to draw.
#[derive(Default)]
pub(crate) struct CalendarTable {
    snapshot: Arc<CalendarSnapshot>,
    /// The choices the header offers, which decide which of the events are rows.
    filter: CalendarFilter,
    /// The days the answer holds, which is what the header's time control offers.
    dates: Vec<Date>,
    /// The country codes the answer holds, which is what its country control offers.
    countries: Vec<SmolStr>,
    /// The days and events of the answer, in the order they are drawn.
    rows: Vec<CalendarRow>,
}

impl CalendarTable {
    /// Replaces everything the table draws with one whole answer, grouped for `zone`.
    ///
    /// A read answers with the whole calendar, so the rows are rebuilt rather than
    /// matched against the ones already there: a day may have gained an event, lost
    /// one, or moved to another day as the setting moved.
    pub(crate) fn rebuild(&mut self, snapshot: Arc<CalendarSnapshot>, zone: UtcOffset) {
        self.snapshot = snapshot;
        self.regroup(zone);
    }

    /// Groups the answer the table already holds for another zone.
    ///
    /// The days an answer holds move with the zone as well, so the options the header
    /// offers are rebuilt here too — and a choice that is no longer one of them goes
    /// back to "all".
    pub(crate) fn regroup(&mut self, zone: UtcOffset) {
        self.dates = dates_of(self.snapshot.events(), zone);
        self.countries = countries_of(self.snapshot.events());
        self.filter.retain_offered(&self.dates, &self.countries);
        self.rows = rows_of(self.snapshot.events(), zone, &self.filter);
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

    /// Drops everything the table draws, which is what a pane that can no longer vouch
    /// for its answer leaves behind.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

impl TableDataSet<COLUMN_COUNT> for CalendarTable {
    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; COLUMN_COUNT] {
        [
            t!(cx, "widget.financial_calendar.importance"),
            t!(cx, "common.time"),
            t!(cx, "widget.financial_calendar.country"),
            t!(cx, "widget.financial_calendar.name"),
        ]
    }

    fn column_aligns(&self) -> [Align; COLUMN_COUNT] {
        [Align::Start; COLUMN_COUNT]
    }

    fn column_widths(&self) -> [Length; COLUMN_COUNT] {
        COLUMN_WIDTHS
    }

    /// The label of a day is one cell across the whole table: it heads the events
    /// below it rather than saying anything about one of the columns.
    fn column_span(&self, row_index: usize, column_index: usize) -> usize {
        match self.rows.get(row_index) {
            Some(CalendarRow::Day(_)) if column_index == IMPORTANCE => COLUMN_COUNT,
            _ => 1,
        }
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        let Some(row) = self.rows.get(row_index) else {
            return Empty.into_any_element();
        };
        let event = match row {
            CalendarRow::Day(date) => return day_label(*date, cx),
            CalendarRow::Event(index) => self.snapshot.events().get(*index),
        };
        let Some(event) = event else {
            return Empty.into_any_element();
        };

        match column_index {
            TIME => time_cell(event, cx),
            COUNTRY => country_cell(event, cx),
            NAME => name_cell(event, cx),
            IMPORTANCE => importance_cell(event, cx),
            _ => Empty.into_any_element(),
        }
    }

    /// A calendar with nothing in it is a fact rather than a state: the table says so
    /// under its headers instead of leaving the body blank.
    fn empty(&self, cx: &App) -> Option<AnyElement> {
        let key = empty_message_key(&self.filter);

        Some(EmptyState::new("FinancialCalendarEmpty", t!(cx, key)).into_any_element())
    }
}

/// The message a table with no row states.
///
/// An answer that carried no event and an answer the header narrowed to nothing are
/// two different facts, and a reader who chose a filter is owed the second one: "no
/// data" would read as if the calendar itself were empty.
fn empty_message_key(filter: &CalendarFilter) -> &'static str {
    if filter.is_all() {
        "common.no_data"
    } else {
        "common.no_matches"
    }
}

/// The head of a day: the day the events below it fall on, in the language showing
/// now.
fn day_label(date: Date, cx: &App) -> AnyElement {
    let theme = cx.theme();

    div()
        .debug_selector(|| DAY_SELECTOR.into())
        .flex()
        .items_center()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.roles.fg.muted.hsla)
        .child(day_label_text(date, cx))
        .into_any_element()
}

/// The day a label heads, written the way the language showing writes a date.
///
/// The header's time control offers the same days and draws them with this, so the day
/// a reader picks above the table is the day the table heads its events with.
pub(crate) fn day_label_text(date: Date, cx: &App) -> SharedString {
    let weekday = t!(cx, weekday_key(date.weekday()));
    let month = t!(cx, month_key(date.month()));

    t!(
        cx,
        "widget.financial_calendar.day_label",
        weekday = weekday,
        month = month,
        day = date.day(),
        year = date.year()
    )
}

/// When an event is due, on the clock the settings display data in.
///
/// The clock is read in a muted tone so the name beside it is what a row reads as:
/// the day is the label above the row, and the time is the part of the row a reader
/// skims rather than reads. An all-day event has no clock, so its cell is empty.
fn time_cell(event: &CalendarEvent, cx: &App) -> AnyElement {
    let theme = cx.theme();

    div()
        .text_color(theme.roles.fg.muted.hsla)
        .child(format_time(event, data_time_zone(cx)))
        .into_any_element()
}

/// The country an event belongs to.
///
/// The code is what the service publishes and what a reader of a calendar scans for,
/// so it is drawn as it arrives — upper-cased — rather than translated into a country
/// name the answer does not carry.
fn country_cell(event: &CalendarEvent, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let text = if event.country.is_empty() {
        SharedString::new_static("—")
    } else {
        SharedString::new(event.country.as_str())
    };

    div()
        .text_color(theme.roles.fg.muted.hsla)
        .child(text)
        .into_any_element()
}

/// What happened, with the period the reading is for beside it.
fn name_cell(event: &CalendarEvent, cx: &App) -> AnyElement {
    let theme = cx.theme();

    div()
        .flex()
        .items_center()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .min_w_0()
                .truncate()
                .child(SharedString::new(event.name.as_str())),
        )
        .children(event.reference.as_ref().map(|reference| {
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.roles.fg.subtle.hsla)
                .child(SharedString::new(reference.as_str()))
        }))
        .into_any_element()
}

/// How much the event matters: one mark per level, and none where the page states
/// no importance.
fn importance_cell(event: &CalendarEvent, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let color = match event.importance {
        Importance::Unspecified => theme.roles.fg.subtle.hsla,
        Importance::Low => theme.roles.status.success.base.hsla,
        Importance::Medium => theme.roles.status.warning.base.hsla,
        Importance::High => theme.roles.status.danger.base.hsla,
    };

    div()
        .flex()
        .items_center()
        .gap_1()
        .children((0..event.importance.marks()).map(|_| {
            div()
                .flex_none()
                .size(px(MARK_SIZE))
                .rounded_full()
                .bg(color)
        }))
        .into_any_element()
}

/// The message key a month is named by.
fn month_key(month: Month) -> &'static str {
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
fn weekday_key(weekday: Weekday) -> &'static str {
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
    use std::collections::HashSet;

    use i18n::language::codes::LanguageCode;

    use super::*;
    use crate::test_support::{boundary_snapshot, snapshot};

    fn table(snapshot: Arc<CalendarSnapshot>, zone: UtcOffset) -> CalendarTable {
        let mut table = CalendarTable::default();
        table.rebuild(snapshot, zone);

        table
    }

    /// The day of October 2026 a test filters by.
    fn day(day: u8) -> Date {
        Date::from_calendar_date(2026, Month::October, day).expect("a day of October 2026")
    }

    #[test]
    fn a_table_of_one_day_should_head_it_once() {
        let table = table(snapshot(3), UtcOffset::UTC);

        assert_eq!(
            table.rows_count(),
            4,
            "three events of one UTC day are one label and three rows"
        );
    }

    /// A read answers with the whole calendar, so a second answer of another length is
    /// a table of another length — not the first one with a row patched.
    #[test]
    fn an_answer_should_replace_every_row_it_replaces() {
        let mut table = table(snapshot(3), UtcOffset::UTC);
        assert_eq!(table.rows_count(), 4);

        table.rebuild(snapshot(1), UtcOffset::UTC);

        assert_eq!(table.rows_count(), 2, "one label and the one event left");
    }

    /// The zone is the whole of what regrouping is: the same answer is two days in UTC
    /// and one day eight hours east of it.
    #[test]
    fn a_zone_should_regroup_the_answer_the_table_already_holds() {
        let mut table = table(boundary_snapshot(), UtcOffset::UTC);
        assert_eq!(table.rows_count(), 4, "two labels and two events");

        table.regroup(UtcOffset::from_hms(8, 0, 0).expect("an offered offset"));

        assert_eq!(
            table.rows_count(),
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

        assert_eq!(table.rows_count(), 2, "one label and the one event left");
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
        assert_eq!(table.rows_count(), 2, "one label and the one German event");

        table.rebuild(boundary_snapshot(), UtcOffset::UTC);

        assert_eq!(
            table.filter().country,
            None,
            "the new answer holds no German event"
        );
        assert_eq!(
            table.rows_count(),
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
        assert_eq!(table.rows_count(), 2, "the late event of the 25th alone");

        table.regroup(UtcOffset::from_hms(8, 0, 0).expect("an offered offset"));

        assert_eq!(table.filter().date, None);
        assert_eq!(
            table.rows_count(),
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
        assert_eq!(table.rows_count(), 0);
        assert!(table.dates().is_empty());
        assert!(table.countries().is_empty());
    }

    #[test]
    fn resetting_the_choices_should_draw_every_event_again() {
        let mut table = table(snapshot(3), UtcOffset::UTC);
        table.select_country(Some(SmolStr::new_static("US")), UtcOffset::UTC);
        assert_eq!(
            table.rows_count(),
            3,
            "one label and the two American events"
        );

        table.reset_filter(UtcOffset::UTC);

        assert!(table.filter().is_all());
        assert_eq!(table.rows_count(), 4);
    }

    /// A table narrowed to nothing says the filter matched nothing; one whose answer
    /// carried nothing says there is no data. The two are different facts.
    #[test]
    fn an_empty_table_should_name_the_reason_it_is_empty() {
        assert_eq!(
            empty_message_key(&CalendarFilter::default()),
            "common.no_data"
        );
        assert_eq!(
            empty_message_key(&CalendarFilter {
                country: Some(SmolStr::new_static("US")),
                ..CalendarFilter::default()
            }),
            "common.no_matches"
        );
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
        let i18n = i18n::global::embedded::load().expect("the embedded locales load");
        let date = Date::from_calendar_date(2026, Month::October, 25).expect("a real date");
        let label = |language| {
            let weekday = i18n.translate(language, weekday_key(date.weekday()));
            let month = i18n.translate(language, month_key(date.month()));

            i18n.translate_with(
                language,
                "widget.financial_calendar.day_label",
                &[
                    ("weekday", &weekday),
                    ("month", &month),
                    ("day", &date.day()),
                    ("year", &date.year()),
                ],
            )
        };

        assert_eq!(label(LanguageCode::En).as_ref(), "Sunday, October 25, 2026");
        assert_eq!(label(LanguageCode::ZhCn).as_ref(), "2026年10月25日 星期日");
    }

    /// The day a row heads is the day of the zone the table was built for, which is
    /// what the label above the row says.
    #[test]
    fn a_label_should_name_the_day_its_events_fall_on() {
        let table = table(boundary_snapshot(), UtcOffset::UTC);
        let CalendarRow::Day(date) = table.rows[0] else {
            panic!("the first row of an answer heads a day");
        };

        assert_eq!(
            date,
            Date::from_calendar_date(2026, Month::October, 25).expect("a real date")
        );
    }
}

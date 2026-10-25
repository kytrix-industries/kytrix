//! The table a calendar is drawn as: four columns, and a label row above each day.
//!
//! The whole answer is the table's only input, so a read that answers again replaces
//! every row rather than patching the ones that moved: a calendar is scheduled ahead,
//! and a revision can add an event in the middle of a day that is already drawn. What
//! the table keeps between reads is nothing but its snapshot and the rows it was
//! grouped into.
//!
//! Grouping is recomputed whenever the zone the settings display data in moves, which
//! is the one thing outside the data that decides where a day begins. The *text* of
//! each row is resolved by the frame that draws it — the time from the zone showing
//! now, the day label from the language showing now — so moving either is a redraw
//! rather than a new answer.

use std::sync::Arc;

use base_ui::{empty_state::EmptyState, style::theme::ThemeExt, table::TableDataSet};
use gpui_kit::{
    AnyElement, App, Empty, FontWeight, InteractiveElement, IntoElement, Length, ParentElement,
    SharedString, Styled, base::Align, div, px, relative,
};
use i18n::t;
use time::{Date, Month, UtcOffset, Weekday};
use widget::time_zone::data_time_zone;

use crate::model::{
    CalendarEvent, CalendarRow, CalendarSnapshot, Importance, format_time, rows_of,
};

/// The columns, in the order a reader takes a calendar in: when, where, what, and how
/// much it matters.
const COLUMN_COUNT: usize = 4;

/// The column a cell belongs to, named so a draw reads as the column it is.
const TIME: usize = 0;
const COUNTRY: usize = 1;
const NAME: usize = 2;
const IMPORTANCE: usize = 3;

/// Column widths in header order.
///
/// The name is what a row is read for, so it takes most of the width; the time, the
/// country and the marks are fixed small parts of it. Every width is a share of the
/// pane, so a narrow pane narrows the name rather than clipping a neighbouring column
/// — the time keeps room for a five-character clock, and the marks for three dots.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.14)),
    Length::Definite(relative(0.09)),
    Length::Definite(relative(0.59)),
    Length::Definite(relative(0.18)),
];

/// The mark an importance is drawn with.
const MARK_SIZE: f32 = 6.0;

/// The day label's debug selector, which is what says a frame drew the head of a day.
const DAY_SELECTOR: &str = "TEST_FINANCIAL_CALENDAR_DAY";

/// The rows of one answer, grouped by the day each event falls on.
///
/// A table with no answer yet is the default one: no snapshot, and no row to draw.
#[derive(Default)]
pub(crate) struct CalendarTable {
    snapshot: Arc<CalendarSnapshot>,
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
        self.rows = rows_of(snapshot.events(), zone);
        self.snapshot = snapshot;
    }

    /// Groups the answer the table already holds for another zone.
    pub(crate) fn regroup(&mut self, zone: UtcOffset) {
        self.rows = rows_of(self.snapshot.events(), zone);
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
            t!(cx, "common.time"),
            t!(cx, "widget.financial_calendar.country"),
            t!(cx, "widget.financial_calendar.name"),
            t!(cx, "widget.financial_calendar.importance"),
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
            Some(CalendarRow::Day(_)) if column_index == TIME => COLUMN_COUNT,
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
        Some(EmptyState::new("FinancialCalendarEmpty", t!(cx, "common.no_data")).into_any_element())
    }
}

/// The head of a day: the day the events below it fall on, in the language showing
/// now.
fn day_label(date: Date, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let weekday = t!(cx, weekday_key(date.weekday()));
    let month = t!(cx, month_key(date.month()));

    div()
        .debug_selector(|| DAY_SELECTOR.into())
        .flex()
        .items_center()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.roles.fg.muted.hsla)
        .child(t!(
            cx,
            "widget.financial_calendar.day_label",
            weekday = weekday,
            month = month,
            day = date.day(),
            year = date.year()
        ))
        .into_any_element()
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
        Importance::Low => theme.roles.fg.subtle.hsla,
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

    #[test]
    fn the_label_of_a_day_should_stand_for_every_column() {
        let table = table(snapshot(1), UtcOffset::UTC);

        assert_eq!(table.column_span(0, TIME), COLUMN_COUNT);
        assert_eq!(
            table.column_span(0, COUNTRY),
            1,
            "only the first column is asked"
        );
        assert_eq!(
            table.column_span(1, TIME),
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

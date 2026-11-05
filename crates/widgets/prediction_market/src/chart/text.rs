//! Everything that turns chart values into shaped text.
//!
//! Shaping is the expensive part of drawing this chart: a frame draws up to a dozen
//! axis labels and, while the pointer is over it, one label per series plus the
//! hovered time. Nothing about those changes while the pointer stays put, so shaped
//! lines are cached for the last [`TEXT_CACHE_CAPACITY`] combinations of text, size,
//! weight and colour. A cache miss is the pointer moving onto a new sample; a cache
//! hit is everything else.
//!
//! Formatting lives here too, and it is deliberately pure: a timestamp, the label
//! unit chosen with the axis step, a zone and the catalogue in, strings out. The
//! language showing decides the month spelling, the date order and whether the clock
//! is a twelve- or twenty-four-hour one, so switching the interface language relabels
//! the axis and the hovered time on the next frame. The year appears only where the
//! chosen unit needs it: the month labels of a multi-year window.

use std::sync::Arc;

use gpui_kit::{FontWeight, Hsla, Pixels, ShapedLine, SharedString, TextRun, TextStyle, Window};
use i18n::{language::codes::LanguageCode, translator::types::I18n};
use smol_str::{SmolStr, format_smolstr};
use time::OffsetDateTime;

use super::geometry::{TimeLabelKind, local_datetime};

/// The most shaped lines the cache holds.
pub(crate) const TEXT_CACHE_CAPACITY: usize = 256;

/// Axis labels: the y values and the x times.
pub(crate) const AXIS_FONT_SIZE: f32 = 12.0;
/// The hover label boxes' text.
pub(crate) const LABEL_FONT_SIZE: f32 = 12.0;
/// The hovered time in the strip above the plot.
pub(crate) const TIME_LABEL_FONT_SIZE: f32 = 12.0;

/// The weight the hover's own labels are set in; the axes stay regular.
pub(crate) const STRONG_WEIGHT: FontWeight = FontWeight::SEMIBOLD;

/// The message key each month is named by.
const MONTH_KEYS: [&str; 12] = [
    "widget.prediction_market.month.january",
    "widget.prediction_market.month.february",
    "widget.prediction_market.month.march",
    "widget.prediction_market.month.april",
    "widget.prediction_market.month.may",
    "widget.prediction_market.month.june",
    "widget.prediction_market.month.july",
    "widget.prediction_market.month.august",
    "widget.prediction_market.month.september",
    "widget.prediction_market.month.october",
    "widget.prediction_market.month.november",
    "widget.prediction_market.month.december",
];

/// One font a chart line is shaped in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartFont {
    pub size: Pixels,
    pub weight: FontWeight,
    pub color: Hsla,
}

impl ChartFont {
    /// A font at the window's own family, in the given size, weight and colour.
    pub(crate) fn new(size: f32, weight: FontWeight, color: Hsla) -> Self {
        Self {
            size: gpui_kit::px(size),
            weight,
            color,
        }
    }
}

/// A line of text shaped once and kept.
struct CachedLine {
    text: SharedString,
    font: ChartFont,
    line: Arc<ShapedLine>,
}

/// Shaped labels, keyed by their text and font.
pub(crate) struct ChartTextCache {
    text_style: Option<TextStyle>,
    entries: Vec<CachedLine>,
}

impl Default for ChartTextCache {
    fn default() -> Self {
        Self {
            text_style: None,
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }
}

impl ChartTextCache {
    /// Shapes `text` in `font`, or returns the line that was shaped before.
    ///
    /// The window's text style is part of the identity of every line: a family or
    /// rem-size change makes the cached lines wrong even though no number moved, so
    /// the whole cache is dropped when it changes.
    pub(crate) fn line(
        &mut self,
        text: SharedString,
        font: ChartFont,
        window: &Window,
    ) -> Arc<ShapedLine> {
        let text_style = window.text_style();
        if self.text_style.as_ref() != Some(&text_style) {
            self.text_style = Some(text_style.clone());
            self.entries.clear();
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.font == font)
        {
            // Moving the entry to the back is what makes the capacity evict the
            // least recently used label.
            let entry = self.entries.remove(index);
            let line = entry.line.clone();
            self.entries.push(entry);
            return line;
        }

        let text_len = text.len();
        let line = Arc::new(window.text_system().shape_line(
            text.clone(),
            font.size,
            &[TextRun {
                len: text_len,
                font: gpui_kit::Font {
                    weight: font.weight,
                    ..text_style.font()
                },
                color: font.color,
                ..Default::default()
            }],
            None,
        ));
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedLine {
            text,
            font,
            line: line.clone(),
        });
        line
    }

    /// The width `text` occupies when shaped in `font`.
    pub(crate) fn width(&mut self, text: &str, font: ChartFont, window: &Window) -> Pixels {
        self.line(SharedString::new(text), font, window).width()
    }
}

/// How a timestamp reads on the time axis.
///
/// The unit is the one chosen with the step (see [`TimeLabelKind`]): a clock tier
/// labels the time of day, a day tier the date, and a month tier the month — with the
/// year only where a multi-year window needs it to keep its months apart.
pub(crate) fn format_axis_time(
    timestamp: i64,
    label: TimeLabelKind,
    zone: time::UtcOffset,
    i18n: &I18n,
    language: LanguageCode,
) -> SmolStr {
    let Some(datetime) = local_datetime(timestamp, zone) else {
        return SmolStr::new_static("");
    };
    match label {
        TimeLabelKind::Clock => format_clock(datetime, i18n, language),
        TimeLabelKind::Day => translated(
            i18n,
            language,
            "widget.prediction_market.date.day",
            &[
                ("month", &month_name(datetime, i18n, language)),
                ("day", &datetime.day()),
            ],
        ),
        TimeLabelKind::Month => month_name(datetime, i18n, language),
        TimeLabelKind::MonthYear => translated(
            i18n,
            language,
            "widget.prediction_market.date.month_year",
            &[
                ("month", &month_name(datetime, i18n, language)),
                ("year", &datetime.year()),
            ],
        ),
    }
}

/// The label of every tick of one axis, in the order the ticks were chosen.
///
/// A clock axis is the one tier that can reach a label it has already shown — a
/// window that ends where it began, or one that wraps the clock face — so a repeated
/// clock label is blanked while its tick stays in place for the gridline. The other
/// tiers label monotonic month or date values and are used as they are.
pub(crate) fn format_axis_labels(
    timestamps: &[i64],
    label: TimeLabelKind,
    zone: time::UtcOffset,
    i18n: &I18n,
    language: LanguageCode,
) -> Vec<SmolStr> {
    if label != TimeLabelKind::Clock {
        return timestamps
            .iter()
            .map(|timestamp| format_axis_time(*timestamp, label, zone, i18n, language))
            .collect();
    }
    let mut seen: Vec<(u8, u8)> = Vec::with_capacity(timestamps.len());
    timestamps
        .iter()
        .map(|timestamp| {
            let Some(datetime) = local_datetime(*timestamp, zone) else {
                return SmolStr::new_static("");
            };
            // A clock label is its local hour and minute; equality of those is
            // equality of the string, because the clock format follows both.
            let key = (datetime.hour(), datetime.minute());
            if seen.contains(&key) {
                return SmolStr::new_static("");
            }
            seen.push(key);
            format_clock(datetime, i18n, language)
        })
        .collect()
}

/// How the hovered sample reads in the strip above the plot.
pub(crate) fn format_hover_time(
    timestamp: i64,
    zone: time::UtcOffset,
    i18n: &I18n,
    language: LanguageCode,
) -> SmolStr {
    let Some(datetime) = local_datetime(timestamp, zone) else {
        return SmolStr::new_static("");
    };
    translated(
        i18n,
        language,
        "widget.prediction_market.date.hover",
        &[
            ("month", &month_name(datetime, i18n, language)),
            ("day", &datetime.day()),
            ("time", &format_clock(datetime, i18n, language)),
        ],
    )
}

/// A probability with the one decimal the hover labels always show.
pub(crate) fn format_percent_one_decimal(percent: f64) -> SmolStr {
    format_smolstr!("{percent:.1}%")
}

/// A probability rounded to the whole percent the y axis and the legend use.
pub(crate) fn format_percent_integer(percent: f64) -> SmolStr {
    format_smolstr!("{percent:.0}%")
}

/// A clock time in the language's own shape, e.g. `8:00 AM` or `08:00`.
fn format_clock(datetime: OffsetDateTime, i18n: &I18n, language: LanguageCode) -> SmolStr {
    let hour = datetime.hour();
    let hour12 = match hour {
        0 => 12,
        1..=11 => hour,
        12 => 12,
        _ => hour - 12,
    };
    let hour24 = format_smolstr!("{hour:02}");
    let minute = format_smolstr!("{:02}", datetime.minute());
    let meridiem = i18n.translate(
        language,
        if hour < 12 {
            "widget.prediction_market.date.am"
        } else {
            "widget.prediction_market.date.pm"
        },
    );
    translated(
        i18n,
        language,
        "widget.prediction_market.date.clock",
        &[
            ("hour12", &hour12),
            ("hour24", &hour24),
            ("minute", &minute),
            ("meridiem", &meridiem),
        ],
    )
}

/// The month's name in the language showing.
fn month_name(datetime: OffsetDateTime, i18n: &I18n, language: LanguageCode) -> SmolStr {
    SmolStr::new(
        i18n.translate(language, MONTH_KEYS[datetime.month() as usize - 1])
            .as_str(),
    )
}

/// One catalogue template with its arguments filled in.
fn translated(
    i18n: &I18n,
    language: LanguageCode,
    key: &'static str,
    arguments: &[(&str, &dyn std::fmt::Display)],
) -> SmolStr {
    SmolStr::new(i18n.translate_with(language, key, arguments).as_str())
}

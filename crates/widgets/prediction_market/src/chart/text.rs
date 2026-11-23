//! Everything that turns chart values into shaped text.
//!
//! Shaping is the expensive part of drawing this chart: a frame draws up to a dozen
//! axis labels and, while the pointer is over it, one label per series plus the
//! hovered time. Nothing about those changes while the pointer stays put, so shaped
//! lines are cached for the last [`TEXT_CACHE_CAPACITY`] combinations of text and
//! style. A cache miss is the pointer moving onto a new sample; a cache hit is
//! everything else.
//!
//! Formatting lives here too, and it is deliberately pure: a timestamp, the label
//! unit chosen with the axis step, a zone and the translation context in, strings out.
//! The language showing decides the month spelling, the date order and whether the
//! clock is a twelve- or twenty-four-hour one, so switching the interface language
//! relabels the axis and the hovered time on the next frame. The year appears only
//! where the chosen unit needs it: the month labels of a multi-year window.
//!
//! # The atlas lease
//!
//! A shaped line is a handle that leases glyph tiles from the process-wide atlas. The
//! engine refuses to encode a run whose lease is gone (`Canvas::push_shaped_text`
//! answers `false`), so a handle that has been evicted must be shaped again rather
//! than drawn: the cache checks [`ShapedTextHandle::is_stale`] on every hit, and the
//! chart's whole-frame short circuit is guarded by the handles the canvas on screen
//! was actually built from.

use std::sync::Arc;

use i18n::translate::Translate;
use smol_str::{SmolStr, format_smolstr};
use time::OffsetDateTime;
use ui_core::{
    app::App,
    paint::color::Color,
    text::{ShapedTextHandle, TextAlign, TextStyle},
};

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
pub(crate) const STRONG_WEIGHT: u16 = 600;
/// The weight the axis labels are set in.
pub(crate) const REGULAR_WEIGHT: u16 = 400;

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

/// One shaped line, kept until its lease is gone or it falls out of the cache.
struct CachedText {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
}

/// Shaped labels, keyed by their text and the style they were shaped in.
pub(crate) struct ChartTextCache {
    entries: Vec<CachedText>,
}

impl Default for ChartTextCache {
    fn default() -> Self {
        Self {
            entries: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }
}

impl ChartTextCache {
    /// Shapes `text` in `style`, or returns the line that was shaped before.
    ///
    /// A handle whose atlas lease is gone is dropped and shaped again: the glyphs it
    /// named have been reused, so drawing it would be drawing another line's pixels.
    /// The style is part of the identity of every line — a weight, a size or a colour
    /// change makes the cached line wrong even though its text did not move.
    pub(crate) fn line(
        &mut self,
        app: &mut App,
        text: &str,
        style: &TextStyle,
    ) -> Arc<ShapedTextHandle> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
        {
            // Refreshing recency by moving the entry to the back is what makes the
            // capacity below evict the least recently used label.
            let entry = self.entries.remove(index);
            if !entry.handle.is_stale() {
                let handle = Arc::clone(&entry.handle);
                self.entries.push(entry);
                return handle;
            }
        }
        let handle = Arc::new(app.shape_text(text, style, None, &[]).unwrap_or_else(|_| {
            // Text that cannot be shaped at all is drawn as nothing rather than
            // failing the frame: an axis label is not worth a dropped frame, and the
            // next frame tries again.
            app.shape_text("", style, None, &[])
                .expect("an empty line always shapes")
        }));
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedText {
            text: SmolStr::new(text),
            style: style.clone(),
            handle: Arc::clone(&handle),
        });
        handle
    }

    /// The width `text` occupies when shaped in `style`.
    pub(crate) fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> f32 {
        self.line(app, text, style).size().width
    }

    /// How many lines the cache holds.
    #[cfg(test)]
    pub(crate) fn cached_len(&self) -> usize {
        self.entries.len()
    }
}

/// The style one chart label is shaped in.
///
/// Alignment is *not* part of it: this chart places every label itself, from the
/// measured width the shaped line reports, so that a label sits where its own box says
/// it does rather than where a wrapping rule would put it.
pub(crate) fn label_style(
    font_size: f32,
    line_height: f32,
    color: Color,
    weight: u16,
) -> TextStyle {
    let mut style = TextStyle::new(font_size);
    style.color = color;
    style.weight = weight;
    style.line_height = Some(line_height / font_size);
    style.alignment = TextAlign::Start;
    style
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
    translate: &Translate,
) -> SmolStr {
    let Some(datetime) = local_datetime(timestamp, zone) else {
        return SmolStr::new_static("");
    };
    match label {
        TimeLabelKind::Clock => format_clock(datetime, translate),
        TimeLabelKind::Day => translate.translate_with(
            "widget.prediction_market.date.day",
            &[
                ("month", &month_name(datetime, translate)),
                ("day", &datetime.day()),
            ],
        ),
        TimeLabelKind::Month => month_name(datetime, translate),
        TimeLabelKind::MonthYear => translate.translate_with(
            "widget.prediction_market.date.month_year",
            &[
                ("month", &month_name(datetime, translate)),
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
    translate: &Translate,
) -> Vec<SmolStr> {
    if label != TimeLabelKind::Clock {
        return timestamps
            .iter()
            .map(|timestamp| format_axis_time(*timestamp, label, zone, translate))
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
            format_clock(datetime, translate)
        })
        .collect()
}

/// How the hovered sample reads in the strip above the plot.
pub(crate) fn format_hover_time(
    timestamp: i64,
    zone: time::UtcOffset,
    translate: &Translate,
) -> SmolStr {
    let Some(datetime) = local_datetime(timestamp, zone) else {
        return SmolStr::new_static("");
    };
    translate.translate_with(
        "widget.prediction_market.date.hover",
        &[
            ("month", &month_name(datetime, translate)),
            ("day", &datetime.day()),
            ("time", &format_clock(datetime, translate)),
        ],
    )
}

/// A probability with the one decimal the hover labels always show.
pub(crate) fn format_percent_one_decimal(percent: f64) -> SmolStr {
    format_smolstr!("{percent:.1}%")
}

/// A probability rounded to the whole percent the y axis uses.
pub(crate) fn format_percent_integer(percent: f64) -> SmolStr {
    format_smolstr!("{percent:.0}%")
}

/// A clock time in the language's own shape, e.g. `8:00 AM` or `08:00`.
fn format_clock(datetime: OffsetDateTime, translate: &Translate) -> SmolStr {
    let hour = datetime.hour();
    let hour12 = match hour {
        0 => 12,
        1..=11 => hour,
        12 => 12,
        _ => hour - 12,
    };
    let hour24 = format_smolstr!("{hour:02}");
    let minute = format_smolstr!("{:02}", datetime.minute());
    let meridiem = translate.translate(if hour < 12 {
        "widget.prediction_market.date.am"
    } else {
        "widget.prediction_market.date.pm"
    });
    translate.translate_with(
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
fn month_name(datetime: OffsetDateTime, translate: &Translate) -> SmolStr {
    translate.translate(MONTH_KEYS[datetime.month() as usize - 1])
}

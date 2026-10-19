//! Everything that turns chart values into shaped text.
//!
//! Shaping is the expensive part of drawing these charts: a frame draws a dozen
//! axis labels, half a dozen time labels, a legend and a tooltip, and none of it
//! changes while the pointer stays put. Two things follow, and they are the whole
//! design of this module:
//!
//! - **labels are formatted once**, when a snapshot arrives, and stored as text
//!   on the scale or the tooltip that owns them. A frame that repainted the same
//!   chart formats nothing;
//! - **shaped lines are cached** for the last [`TEXT_CACHE_CAPACITY`]
//!   `(text, size, colour)` triples, so a tooltip that moved one interval
//!   re-shapes one line rather than five.
//!
//! The cache is keyed on the window's text style as well as on its own inputs: a
//! style or font change invalidates shaped text even when no number moved.

use std::sync::Arc;

use gpui_kit::{Hsla, Pixels, ShapedLine, SharedString, TextRun, TextStyle, Window};
use market_participants_model::decimal::{Price, Quantity, Tick};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

pub(crate) const TEXT_CACHE_CAPACITY: usize = 192;
/// The decimal scale every value in `market_participants_model` is stored at.
pub(crate) const DECIMAL_SCALE: f64 = 1_000_000_000_000_000_000.0;

/// A line of text shaped once and kept.
pub(crate) struct CachedTextLine {
    text: SharedString,
    font_size: Pixels,
    color: Hsla,
    line: Arc<ShapedLine>,
}

/// Shaped labels, keyed by text, size and colour.
pub(crate) struct ChartTextCache {
    text_style: Option<TextStyle>,
    entries: Vec<CachedTextLine>,
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
    /// Shapes `text` in `color`, or returns the line that was shaped before.
    pub(crate) fn line(
        &mut self,
        text: SharedString,
        font_size: Pixels,
        color: Hsla,
        window: &Window,
    ) -> Arc<ShapedLine> {
        let text_style = window.text_style();
        if self.text_style.as_ref() != Some(&text_style) {
            self.text_style = Some(text_style.clone());
            self.entries.clear();
        }
        if let Some(index) = self.entries.iter().position(|entry| {
            entry.text == text && entry.color == color && entry.font_size == font_size
        }) {
            // Refreshing recency by moving the entry to the back is what makes
            // the capacity below evict the least recently used label.
            let entry = self.entries.remove(index);
            let line = entry.line.clone();
            self.entries.push(entry);
            return line;
        }

        let text_len = text.len();
        let line = Arc::new(window.text_system().shape_line(
            text.clone(),
            font_size,
            &[TextRun {
                len: text_len,
                font: text_style.font(),
                color,
                ..Default::default()
            }],
            None,
        ));
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedTextLine {
            text,
            font_size,
            color,
            line: line.clone(),
        });
        line
    }

    /// The width `text` occupies, shaped in `color`.
    pub(crate) fn width(
        &mut self,
        text: &str,
        font_size: Pixels,
        color: Hsla,
        window: &Window,
    ) -> Pixels {
        self.line(SharedString::new(text), font_size, color, window)
            .width()
    }
}

/// A value formatted at the instrument's own tick.
///
/// This is what separates a label a trader can use from a label that merely
/// contains the number: a value rounded to the tick the venue quotes is the
/// precision the venue itself would have printed.
///
/// The digits are grouped because an open interest is read by its magnitude first:
/// `1234567.00` has to be counted, and `1,234,567.00` is seen. What is grouped is
/// the integer part only — a tick's own decimals are never far enough from the
/// point to need a separator.
pub(crate) fn format_quantity(value: f64, tick: Tick) -> SmolStr {
    if !value.is_finite() || value < 0.0 {
        return NOT_A_READING;
    }
    decimal_inner(value).map_or_else(
        || compact(value),
        |inner| group_digits(&Quantity::from_inner(inner).format_and_round_tick(tick)),
    )
}

/// A value formatted at the instrument's own price tick.
pub(crate) fn format_price(value: f64, tick: Tick) -> SmolStr {
    if !value.is_finite() || value < 0.0 {
        return NOT_A_READING;
    }
    decimal_inner(value).map_or_else(
        || compact(value),
        |inner| group_digits(&Price::from_inner(inner).format_and_round_tick(tick)),
    )
}

/// The integer part of a formatted number, grouped in threes.
///
/// The input is a number as `market_participants_model` writes one — digits, an optional
/// sign, an optional fractional part, and `e` notation for values beyond the
/// model's own scale. Everything except the run of digits before the point is
/// carried over untouched, so this never changes what a number *says*.
pub(crate) fn group_digits(text: &str) -> SmolStr {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (integer, tail) = match rest.find(['.', 'e', 'E']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    if integer.len() <= 3 || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return SmolStr::new(text);
    }
    let mut grouped = String::with_capacity(text.len() + integer.len() / 3);
    grouped.push_str(sign);
    for (index, digit) in integer.bytes().enumerate() {
        if index > 0 && (integer.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(digit));
    }
    grouped.push_str(tail);
    SmolStr::new(grouped)
}

/// What a value that is not a reading is shown as.
///
/// A quantity that is negative or infinite is not a small one, so it is named
/// rather than printed: an axis that formatted it would put a number on screen
/// that the venue never published.
pub(crate) const NOT_A_READING: SmolStr = SmolStr::new_static("--");

/// A dimensionless value, for a series that has no instrument behind it.
///
/// Three decimals rather than two: a long/short ratio is a quotient of two large
/// numbers, and the third decimal is where its movement between two intervals is
/// usually visible — at two, neighbouring readings print as the same number. The
/// value is also what a ratio axis labels its ticks with, so the reading and the
/// axis it is read against keep the same precision.
pub(crate) fn format_ratio(value: f64) -> SmolStr {
    if value != 0.0 && (value.abs() < 0.0001 || value.abs() >= 1_000_000_000.0) {
        return format_smolstr!("{value:.2e}");
    }
    format_smolstr!("{value:.3}")
}

/// A share of a whole, as a percentage.
pub(crate) fn format_share(value: f64) -> SmolStr {
    format_smolstr!("{:.2}%", value * 100.0)
}

/// The instrument's value as the 18-digit fixed point the model stores.
pub(crate) fn decimal_inner(value: f64) -> Option<u128> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let inner = (value * DECIMAL_SCALE).round();
    inner
        .is_finite()
        .then_some(inner.min(u128::MAX as f64) as u128)
}

/// A value written short enough to fit an axis, for a series that has no
/// instrument to take its precision from.
///
/// The tooltip names one interval's value rather than comparing it with the one
/// above, so it has no step to keep clear of: passing the value itself as the step
/// is what lets the largest readable suffix be used.
pub(crate) fn compact(value: f64) -> SmolStr {
    format_compact(value, value)
}

/// [`compact`] with the tick spacing the label has to stay distinct across.
///
/// Two neighbouring grid lines have to read as two different numbers, which is
/// the one thing a fixed number of decimals cannot promise. So the suffix is
/// chosen as the largest one that still leaves ten steps of room beneath it: a
/// suffix that divides a whole step away would print both of its ends as the same
/// label, and a label that is short but wrong is worse than a long one.
pub(crate) fn format_compact(value: f64, step: f64) -> SmolStr {
    let absolute = value.abs();
    if absolute >= DECIMAL_SCALE {
        return format_smolstr!("{value:.2e}");
    }
    let step = step.abs();
    let (divisor, suffix) = if absolute >= 1_000_000_000_000.0 && step >= 100_000_000_000.0 {
        (1_000_000_000_000.0, "T")
    } else if absolute >= 1_000_000_000.0 && step >= 100_000_000.0 {
        (1_000_000_000.0, "B")
    } else if absolute >= 1_000_000.0 && step >= 100_000.0 {
        (1_000_000.0, "M")
    } else if absolute >= 1_000.0 && step >= 100.0 {
        (1_000.0, "K")
    } else {
        (1.0, "")
    };
    let scaled = value / divisor;
    let scaled_step = step / divisor;
    let precision = if !scaled_step.is_finite() || scaled_step <= 0.0 || scaled_step >= 1.0 {
        0
    } else {
        (-scaled_step.log10()).ceil().clamp(0.0, 6.0) as usize
    };
    let mut text = format!("{scaled:.precision$}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    format_smolstr!("{text}{suffix}")
}

/// A value the way a tooltip writes it: what it is, then the number.
pub(crate) fn labelled_value(label: &str, value: impl std::fmt::Display) -> SmolStr {
    format_smolstr!("{label}: {value}")
}

/// A label with the asset it is measured in.
pub(crate) fn label_with_unit(label: &str, unit: &str) -> SmolStr {
    format_smolstr!("{label} ({unit})")
}

/// The time axis' own format: the interval, without a year.
///
/// The axis spans hours at most, so the date is only there to say when the window
/// starts; the year is the tooltip's business, and seconds are not a reading either
/// of them shows. Both read the instant in the zone the settings display data in.
pub(crate) fn format_axis_time(timestamp: i64, zone: UtcOffset) -> SmolStr {
    datetime(timestamp, zone).map_or_else(
        || SmolStr::new_static("--"),
        |datetime| {
            format_smolstr!(
                "{:02}-{:02} {:02}:{:02}",
                u8::from(datetime.month()),
                datetime.day(),
                datetime.hour(),
                datetime.minute()
            )
        },
    )
}

/// The tooltip's own format: the exact interval, in the zone the settings display data in.
pub(crate) fn format_tooltip_time(timestamp: i64, zone: UtcOffset) -> SmolStr {
    datetime(timestamp, zone).map_or_else(
        || SmolStr::new_static("--"),
        |datetime| {
            format_smolstr!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                datetime.year(),
                u8::from(datetime.month()),
                datetime.day(),
                datetime.hour(),
                datetime.minute()
            )
        },
    )
}

/// The instant `timestamp` names, read in `zone`, or nothing when either the
/// timestamp or its local reading is outside the calendar.
///
/// The offset can move an instant past the last date the calendar holds, and a
/// label formatted from that would panic: an instant off the end of the range is
/// one this cannot read.
pub(crate) fn datetime(timestamp: i64, zone: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
}

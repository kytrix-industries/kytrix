//! Everything that turns chart values into shaped text.
//!
//! Shaping is the expensive part of drawing these charts: a frame draws a dozen axis
//! labels, half a dozen time labels, a legend and a tooltip, and none of it changes
//! while the pointer stays put. Two things follow, and they are the whole design of
//! this module:
//!
//! - **labels are formatted once**, when a snapshot arrives, and stored as text on
//!   the scale or the point that owns them. A frame that repainted the same chart
//!   formats nothing;
//! - **shaped text is cached** for the last [`TEXT_CACHE_CAPACITY`]
//!   `(text, style)` pairs, so a tooltip that moved one interval re-shapes one line
//!   rather than five.
//!
//! A shaped handle carries an atlas lease: the glyphs it points at can be evicted (an
//! atlas generation change, or a per-tile LRU sweep), and a handle whose lease is gone
//! is re-shaped here rather than drawn from stale texture coordinates. Two things
//! keep a dead lease from quietly taking a label off the screen, and they are the two
//! halves of the same promise:
//!
//! - the cache checks [`ShapedTextHandle::is_stale`] on every hit, and it answers
//!   [`ChartTextCache::has_stale`] so the chart's whole-frame short-circuit cannot
//!   decide "nothing changed" over a cache of dead handles;
//! - a line the engine refuses while the canvas is being built
//!   (`Canvas::push_shaped_text` answers `false`) makes that canvas incomplete, and
//!   the frame does not count as what the chart draws: the next one shapes and pushes
//!   again rather than leaving the missing run missing.

use std::sync::Arc;

use base_ui::format::group_digits;
use market_participants_model::decimal::{Price, Quantity, Tick};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};
use ui_core::{
    app::App,
    paint::color::Color,
    text::{ShapedTextHandle, TextAlign, TextStyle},
};

/// How many shaped labels one chart keeps.
pub(crate) const TEXT_CACHE_CAPACITY: usize = 192;
/// The decimal scale every value in `market_participants_model` is stored at.
pub(crate) const DECIMAL_SCALE: f64 = 1_000_000_000_000_000_000.0;

/// What a value that is not a reading is shown as.
///
/// A quantity that is negative or infinite is not a small one, so it is named rather
/// than printed: an axis that formatted it would put a number on screen that the
/// venue never published.
pub(crate) const NOT_A_READING: SmolStr = SmolStr::new_static("--");

/// One shaped line, kept until its lease is gone or it falls out of the cache.
struct CachedText {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
}

/// Shaped labels, keyed by text and the style they were shaped in.
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

    /// The width `text` occupies, shaped in `style`.
    pub(crate) fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> f32 {
        self.line(app, text, style).size().width
    }

    /// Whether any line this cache holds can no longer be drawn from the atlas, dropping
    /// the ones that cannot.
    ///
    /// This is what the chart asks before it decides a frame changed nothing: a canvas
    /// is a value and the chart keeps the one it built, but the *glyphs* a canvas names
    /// are leased from a shared atlas, and an atlas generation change or a per-tile
    /// eviction leaves every handle the canvas was built from pointing at reused
    /// texture. The engine drops such runs at encode time, so a chart that did not
    /// rebuild would lose its axis, date and tooltip labels permanently.
    ///
    /// The stale entries are dropped on the way out. A label the frame that follows does
    /// not draw against — a tooltip whose pointer has moved away since — would otherwise
    /// keep this answer true for good, which turns every later frame into a rebuild and
    /// breaks the steady state (I3). The canvas that was built from them is rebuilt by
    /// the frame that asks this question, so nothing is left naming glyphs that are gone.
    pub(crate) fn has_stale(&mut self) -> bool {
        let stale = self.entries.iter().any(|entry| entry.handle.is_stale());
        if stale {
            self.entries.retain(|entry| !entry.handle.is_stale());
        }
        stale
    }
}

/// The style one chart label is shaped in.
///
/// Alignment is *not* part of it: this chart places every label itself, from the
/// measured width the shaped line reports, so that a label sits where its own axis
/// says it does rather than where a wrapping rule would put it.
pub(crate) fn label_style(font_size: f32, line_height: f32, color: Color) -> TextStyle {
    let mut style = TextStyle::new(font_size);
    style.color = color;
    style.line_height = Some(line_height / font_size);
    style.alignment = TextAlign::Left;
    style
}

/// A value formatted at the instrument's own tick.
///
/// This is what separates a label a trader can use from a label that merely contains
/// the number: a value rounded to the tick the venue quotes is the precision the venue
/// itself would have printed.
///
/// The digits are grouped because an open interest is read by its magnitude first:
/// `1234567.00` has to be counted, and `1,234,567.00` is seen. What is grouped is the
/// integer part only — a tick's own decimals are never far enough from the point to
/// need a separator.
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

/// A dimensionless value, for a series that has no instrument behind it.
///
/// Three decimals rather than two: a long/short ratio is a quotient of two large
/// numbers, and the third decimal is where its movement between two intervals is
/// usually visible — at two, neighbouring readings print as the same number. The value
/// is also what a ratio axis labels its ticks with, so the reading and the axis it is
/// read against keep the same precision.
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

/// A value written short enough to fit an axis, for a series that has no instrument
/// to take its precision from.
///
/// The tooltip names one interval's value rather than comparing it with the one above,
/// so it has no step to keep clear of: passing the value itself as the step is what
/// lets the largest readable suffix be used.
pub(crate) fn compact(value: f64) -> SmolStr {
    format_compact(value, value)
}

/// [`compact`] with the tick spacing the label has to stay distinct across.
///
/// Two neighbouring grid lines have to read as two different numbers, which is the one
/// thing a fixed number of decimals cannot promise. So the suffix is chosen as the
/// largest one that still leaves ten steps of room beneath it: a suffix that divides a
/// whole step away would print both of its ends as the same label, and a label that is
/// short but wrong is worse than a long one.
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
/// starts; the year is the tooltip's business, and seconds are not a reading either of
/// them shows. Both read the instant in the zone the settings display data in.
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

/// The tooltip's own format: the exact interval, in the zone the settings display data
/// in.
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

/// The instant `timestamp` names, read in `zone`, or nothing when either the timestamp
/// or its local reading is outside the calendar.
///
/// The offset can move an instant past the last date the calendar holds, and a label
/// formatted from that would panic: an instant off the end of the range is one this
/// cannot read.
pub(crate) fn datetime(timestamp: i64, zone: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
}

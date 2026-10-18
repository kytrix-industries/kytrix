//! Everything that turns chart values into shaped text.
//!
//! Shaping is the expensive part of drawing this chart: a frame draws a dozen axis
//! labels, half a dozen time labels and a tooltip, and none of it changes while the
//! pointer stays put. Two things follow, and they are the whole design of this
//! module:
//!
//! - **labels are formatted once**, when a snapshot arrives, and stored as text on
//!   the scale or the point that owns them. A frame that repainted the same chart
//!   formats nothing;
//! - **shaped lines are cached** for the last [`TEXT_CACHE_CAPACITY`]
//!   `(text, size, colour)` triples, so a tooltip that moved one day re-shapes one
//!   line rather than four.
//!
//! The cache is keyed on the window's text style as well as on its own inputs: a
//! style or font change invalidates shaped text even when no number moved.

use std::sync::Arc;

use gpui_kit::{Hsla, Pixels, ShapedLine, SharedString, TextRun, TextStyle, Window};
use smol_str::{SmolStr, format_smolstr};
use time::Date;

pub(crate) const TEXT_CACHE_CAPACITY: usize = 128;

/// What a day the service did not report is shown as.
///
/// A missing reading is not a small one, so it is named rather than printed: an
/// axis or a tooltip that formatted it would put a number on screen that no fund
/// ever published.
pub(crate) const NOT_A_READING: SmolStr = SmolStr::new_static("--");

/// A line of text shaped once and kept.
struct CachedTextLine {
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
            // Refreshing recency by moving the entry to the back is what makes the
            // capacity below evict the least recently used label.
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

/// A number on a y-axis, at the precision its own step needs.
///
/// The step is what decides the decimals: a tick that is ten apart has no use for
/// a hundredth, and one that is a hundredth apart must not be printed as its
/// neighbour. A step of one or more therefore prints whole numbers, which is what
/// every value on these axes is.
pub(crate) fn format_axis_value(value: f64, step: f64) -> SmolStr {
    let absolute_step = step.abs();
    let precision = if absolute_step >= 1.0 {
        0
    } else {
        (-absolute_step.log10()).ceil().clamp(0.0, 4.0) as usize
    };
    format_smolstr!("{value:.precision$}")
}

/// An amount, in the unit the service publishes ETF flow in: millions of dollars.
///
/// The unit is the whole reason for the suffixes. A row of the upstream sheet is a
/// number of millions, so `12.5` is twelve and a half million dollars and `1250` is
/// a billion — printing either as a bare number would be off by six orders of
/// magnitude, and printing the second as `1250M` would make a reader count digits
/// to compare it with the first.
pub(crate) fn format_money(value: f64) -> SmolStr {
    if !value.is_finite() {
        return NOT_A_READING;
    }
    let absolute = value.abs();
    let (scaled, suffix) = if absolute >= 1_000_000.0 {
        (value / 1_000_000.0, "T")
    } else if absolute >= 1_000.0 {
        (value / 1_000.0, "B")
    } else {
        (value, "M")
    };
    if scaled < 0.0 {
        format_smolstr!("-${:.2}{}", scaled.abs(), suffix)
    } else {
        format_smolstr!("${:.2}{}", scaled, suffix)
    }
}

/// An amount that says which way it moved.
///
/// The sign belongs in front of the whole amount rather than inside it: `+$1.25B`
/// is one reading, `$-1.25B` is a minus sign that a reader has to find between two
/// currency marks.
pub(crate) fn format_signed_money(value: f64) -> SmolStr {
    if !value.is_finite() {
        return NOT_A_READING;
    }
    let absolute = format_money(value.abs());
    if value > 0.0 {
        format_smolstr!("+{absolute}")
    } else if value < 0.0 {
        format_smolstr!("-{absolute}")
    } else {
        absolute
    }
}

/// The time axis' own format: the day, without a year.
///
/// A window spans a year at most, and the year a bar belongs to is the window's, so
/// repeating it on every label would cost more room than the date itself needs.
pub(crate) fn format_date(date: Date) -> SmolStr {
    format_smolstr!("{:02}-{:02}", date.month() as u8, date.day())
}

/// The tooltip's own format: the whole trading day, including its year.
pub(crate) fn format_header_date(date: Date) -> SmolStr {
    format_smolstr!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

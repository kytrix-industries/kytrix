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
//! - **shaped text is cached** for the last [`TEXT_CACHE_CAPACITY`]
//!   `(text, size, colour, align)` tuples, so a tooltip that moved one day re-shapes
//!   one line rather than four.
//!
//! A shaped handle carries an atlas lease: the glyphs it points at can be evicted
//! (an atlas generation change, or a per-tile LRU sweep), and a handle whose lease is
//! gone is re-shaped here rather than drawn from stale texture coordinates. The
//! cache therefore checks [`ShapedTextHandle::is_stale`] on every hit, and the canvas
//! that a frame builds is rebuilt whenever the chart redraws — an application that
//! pushed stale glyphs would be told by `Canvas::push_shaped_text` returning `false`,
//! which this module treats as "shape it again".

use std::sync::Arc;

use smol_str::{SmolStr, format_smolstr};
use time::Date;
use ui_core::{
    app::App,
    paint::color::Color,
    text::{ShapedTextHandle, TextAlign, TextStyle},
};

/// How many shaped labels one chart keeps.
pub(crate) const TEXT_CACHE_CAPACITY: usize = 128;

/// What a day the service did not report is shown as.
///
/// A missing reading is not a small one, so it is named rather than printed: an
/// axis or a tooltip that formatted it would put a number on screen that no fund
/// ever published.
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
            // Text that cannot be shaped at all is drawn as nothing rather
            // than failing the frame: an axis label is not worth a dropped
            // frame, and the next frame tries again.
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

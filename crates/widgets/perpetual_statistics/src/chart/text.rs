use market_participants_model::decimal::{Price, Quantity, Tick};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};
use ui_core::{
    app::App,
    math::LogicalPx,
    paint::color::Color,
    text::{TextAlign, TextContent, TextStyle},
};
use ui_kit::kit::format::group_digits;

pub(crate) const TEXT_CACHE_CAPACITY: usize = 192;
pub(crate) const DECIMAL_SCALE: f64 = 1_000_000_000_000_000_000.0;

pub(crate) const NOT_A_READING: SmolStr = SmolStr::new_static("--");

struct CachedText {
    text: SmolStr,
    style: TextStyle,
    width: LogicalPx,
}

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
    pub(crate) fn line(
        &mut self,
        app: &mut App,
        text: &str,
        style: &TextStyle,
    ) -> (TextContent, LogicalPx) {
        let width = self.width(app, text, style);
        (TextContent::new(text, style.clone()), width)
    }

    #[cfg(test)]
    pub(crate) fn content(&mut self, app: &mut App, text: &str, style: &TextStyle) -> TextContent {
        self.line(app, text, style).0
    }
    pub(crate) fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> LogicalPx {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
        {
            let entry = self.entries.remove(index);
            let width = entry.width;
            self.entries.push(entry);
            return width;
        }
        let width = app
            .measure_text(&TextContent::new(text, style.clone()), None)
            .width;
        if self.entries.len() == TEXT_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedText {
            text: SmolStr::new(text),
            style: style.clone(),
            width,
        });
        width
    }
}

pub(crate) fn label_style(font_size: LogicalPx, line_height: LogicalPx, color: Color) -> TextStyle {
    let mut style = TextStyle::new(font_size);
    style.color = color;
    style.line_height = Some(line_height / font_size);
    style.alignment = TextAlign::Left;
    style
}

pub(crate) fn format_quantity(value: f64, tick: Tick) -> SmolStr {
    if !value.is_finite() || value < 0.0 {
        return NOT_A_READING;
    }
    decimal_inner(value).map_or_else(
        || compact(value),
        |inner| group_digits(&Quantity::from_inner(inner).format_and_round_tick(tick)),
    )
}

pub(crate) fn format_price(value: f64, tick: Tick) -> SmolStr {
    if !value.is_finite() || value < 0.0 {
        return NOT_A_READING;
    }
    decimal_inner(value).map_or_else(
        || compact(value),
        |inner| group_digits(&Price::from_inner(inner).format_and_round_tick(tick)),
    )
}

pub(crate) fn format_ratio(value: f64) -> SmolStr {
    if value != 0.0 && (value.abs() < 0.0001 || value.abs() >= 1_000_000_000.0) {
        return format_smolstr!("{value:.2e}");
    }
    format_smolstr!("{value:.3}")
}

pub(crate) fn format_share(value: f64) -> SmolStr {
    format_smolstr!("{:.2}%", value * 100.0)
}

pub(crate) fn decimal_inner(value: f64) -> Option<u128> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let inner = (value * DECIMAL_SCALE).round();
    inner
        .is_finite()
        .then_some(inner.min(u128::MAX as f64) as u128)
}

pub(crate) fn compact(value: f64) -> SmolStr {
    format_compact(value, value)
}

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

pub(crate) fn labelled_value(label: &str, value: impl std::fmt::Display) -> SmolStr {
    format_smolstr!("{label}: {value}")
}

pub(crate) fn label_with_unit(label: &str, unit: &str) -> SmolStr {
    format_smolstr!("{label} ({unit})")
}

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

pub(crate) fn datetime(timestamp: i64, zone: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
}

use smol_str::{SmolStr, format_smolstr};
use time::Date;
use ui_core::{
    app::App,
    math::LogicalPx,
    paint::color::Color,
    text::{TextAlign, TextContent, TextStyle},
};

pub(crate) const TEXT_CACHE_CAPACITY: usize = 128;

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
        if let Some(index) = self.index_of(text, style) {
            let entry = self.entries.remove(index);
            let content = TextContent::new(entry.text.clone(), entry.style.clone());
            let width = entry.width;
            self.entries.push(entry);
            return (content, width);
        }
        let width = self.measure(app, text, style);
        (TextContent::new(text, style.clone()), width)
    }

    pub(crate) fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> LogicalPx {
        if let Some(index) = self.index_of(text, style) {
            let entry = self.entries.remove(index);
            let width = entry.width;
            self.entries.push(entry);
            return width;
        }
        self.measure(app, text, style)
    }

    fn index_of(&self, text: &str, style: &TextStyle) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
    }

    fn measure(&mut self, app: &mut App, text: &str, style: &TextStyle) -> LogicalPx {
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

pub(crate) fn format_axis_value(value: f64, step: f64) -> SmolStr {
    let absolute_step = step.abs();
    let precision = if absolute_step >= 1.0 {
        0
    } else {
        (-absolute_step.log10()).ceil().clamp(0.0, 4.0) as usize
    };
    format_smolstr!("{value:.precision$}")
}

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

pub(crate) fn format_date(date: Date) -> SmolStr {
    format_smolstr!("{:02}-{:02}", date.month() as u8, date.day())
}

pub(crate) fn format_header_date(date: Date) -> SmolStr {
    format_smolstr!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

pub(crate) fn label_style(font_size: LogicalPx, line_height: LogicalPx, color: Color) -> TextStyle {
    let mut style = TextStyle::new(font_size);
    style.color = color;
    style.line_height = Some(line_height.value() / font_size.value());
    style.alignment = TextAlign::Left;
    style
}

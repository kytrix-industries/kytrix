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

pub(crate) const TEXT_CACHE_CAPACITY: usize = 256;

pub(crate) const AXIS_FONT_SIZE: f32 = 12.0;
pub(crate) const LABEL_FONT_SIZE: f32 = 12.0;
pub(crate) const TIME_LABEL_FONT_SIZE: f32 = 12.0;

pub(crate) const STRONG_WEIGHT: u16 = 600;
pub(crate) const REGULAR_WEIGHT: u16 = 400;

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

struct CachedText {
    text: SmolStr,
    style: TextStyle,
    handle: Arc<ShapedTextHandle>,
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
    ) -> Arc<ShapedTextHandle> {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text == text && entry.style == *style)
        {
            let entry = self.entries.remove(index);
            if !entry.handle.is_stale() {
                let handle = Arc::clone(&entry.handle);
                self.entries.push(entry);
                return handle;
            }
        }
        let handle = Arc::new(app.shape_text(text, style, None, &[]).unwrap_or_else(|_| {
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

    pub(crate) fn width(&mut self, app: &mut App, text: &str, style: &TextStyle) -> f32 {
        self.line(app, text, style).size().width
    }

    #[cfg(test)]
    pub(crate) fn cached_len(&self) -> usize {
        self.entries.len()
    }
}

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
            let key = (datetime.hour(), datetime.minute());
            if seen.contains(&key) {
                return SmolStr::new_static("");
            }
            seen.push(key);
            format_clock(datetime, translate)
        })
        .collect()
}

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

pub(crate) fn format_percent_one_decimal(percent: f64) -> SmolStr {
    format_smolstr!("{percent:.1}%")
}

pub(crate) fn format_percent_integer(percent: f64) -> SmolStr {
    format_smolstr!("{percent:.0}%")
}

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

fn month_name(datetime: OffsetDateTime, translate: &Translate) -> SmolStr {
    translate.translate(MONTH_KEYS[datetime.month() as usize - 1])
}

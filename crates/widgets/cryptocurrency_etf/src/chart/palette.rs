use ui_core::{
    math::{LogicalPx, LogicalSize, px},
    paint::color::Color,
};
use ui_kit::theme::{components::ComponentTokens, resolved::Theme};

pub(crate) const AXIS_FONT_SIZE: LogicalPx = px(10.0);
pub(crate) const AXIS_LINE_HEIGHT: LogicalPx = px(14.0);
pub(crate) const LINE_WIDTH: LogicalPx = px(2.0);
pub(crate) const TOOLTIP_GAP: LogicalPx = px(10.0);
pub(crate) const CROSSHAIR_DASH: LogicalPx = px(4.0);
pub(crate) const CROSSHAIR_PERIOD: LogicalPx = px(7.0);
const TOOLTIP_RADIUS: LogicalPx = px(4.0);
const TOOLTIP_PADDING: LogicalPx = px(4.0);
const TOOLTIP_LINE_HEIGHT_RATIO: f32 = 1.4;

pub(crate) fn flow(theme: &Theme, value: f64, muted: Color) -> Color {
    let chart = &theme.components().chart;
    if !value.is_finite() {
        muted
    } else if value < 0.0 {
        chart.bearish
    } else if value > 0.0 {
        chart.bullish
    } else {
        muted
    }
}

pub(crate) fn tooltip_style(tokens: &ComponentTokens) -> TooltipStyle {
    let font_size = tokens.small.font_size;
    TooltipStyle {
        font_size,
        line_height: font_size * TOOLTIP_LINE_HEIGHT_RATIO,
        padding: TOOLTIP_PADDING,
        radius: TOOLTIP_RADIUS,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TooltipStyle {
    pub font_size: LogicalPx,
    pub line_height: LogicalPx,
    pub padding: LogicalPx,
    pub radius: LogicalPx,
}

impl TooltipStyle {
    pub(crate) fn content_size(self, content_width: LogicalPx, line_count: usize) -> LogicalSize {
        LogicalSize::new(
            content_width + self.padding * 2.0,
            self.line_height * line_count as f32 + self.padding * 2.0,
        )
    }
}

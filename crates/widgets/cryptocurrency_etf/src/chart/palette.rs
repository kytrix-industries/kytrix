use base_ui::theme::{components::ComponentTokens, resolved::Theme};
use ui_core::{math::Size, paint::color::Color};

pub(crate) const AXIS_FONT_SIZE: f32 = 10.0;
pub(crate) const AXIS_LINE_HEIGHT: f32 = 14.0;
pub(crate) const LINE_WIDTH: f32 = 2.0;
pub(crate) const TOOLTIP_GAP: f32 = 10.0;
pub(crate) const CROSSHAIR_DASH: f32 = 4.0;
pub(crate) const CROSSHAIR_PERIOD: f32 = 7.0;
const TOOLTIP_RADIUS: f32 = 4.0;
const TOOLTIP_PADDING: f32 = 4.0;
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
    pub font_size: f32,
    pub line_height: f32,
    pub padding: f32,
    pub radius: f32,
}

impl TooltipStyle {
    pub(crate) fn content_size(self, content_width: f32, line_count: usize) -> Size {
        Size::new(
            content_width + self.padding * 2.0,
            self.line_height * line_count as f32 + self.padding * 2.0,
        )
    }
}

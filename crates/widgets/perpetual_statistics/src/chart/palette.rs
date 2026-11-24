use smol_str::SmolStr;
use ui_core::{
    math::{LogicalPx, LogicalSize, px},
    paint::color::Color,
};
use ui_kit::theme::{components::ComponentTokens, resolved::Theme, role::Role};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Series {
    Amount,
    Value,
    Long,
    Short,
    Ratio,
}

impl Series {
    pub(crate) const fn role(self) -> Role {
        match self {
            Self::Amount => Role::ChartSeries0,
            Self::Value => Role::ChartSeries1,
            Self::Long => Role::MarketUp,
            Self::Short => Role::MarketDown,
            Self::Ratio => Role::FgDefault,
        }
    }

    pub(crate) fn color(self, theme: &Theme) -> Color {
        theme.role(self.role())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeriesLabel {
    pub series: Series,
    pub label: SmolStr,
}

pub(crate) const AXIS_FONT_SIZE: LogicalPx = px(10.0);
pub(crate) const AXIS_LINE_HEIGHT: LogicalPx = px(14.0);
pub(crate) const LINE_WIDTH: LogicalPx = px(2.0);
pub(crate) const LINE_MARKER_SIZE: LogicalPx = px(4.0);
pub(crate) const HOVER_MARKER_SIZE: LogicalPx = px(9.0);
pub(crate) const TOOLTIP_GAP: LogicalPx = px(10.0);
pub(crate) const CROSSHAIR_DASH: LogicalPx = px(4.0);
pub(crate) const CROSSHAIR_PERIOD: LogicalPx = px(7.0);
pub(crate) const LEGEND_SWATCH_GAP: LogicalPx = px(20.0);
pub(crate) const LEGEND_ENTRY_GAP: LogicalPx = px(18.0);
pub(crate) const LEGEND_OVERFLOW_INSET: LogicalPx = px(4.0);
const TOOLTIP_RADIUS: LogicalPx = px(4.0);
const TOOLTIP_PADDING: LogicalPx = px(4.0);
const TOOLTIP_LINE_HEIGHT_RATIO: f32 = 1.4;

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

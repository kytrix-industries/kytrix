use base_ui::theme::{components::ComponentTokens, resolved::Theme, role::Role};
use smol_str::SmolStr;
use ui_core::{math::Size, paint::color::Color};

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

pub(crate) const AXIS_FONT_SIZE: f32 = 10.0;
pub(crate) const AXIS_LINE_HEIGHT: f32 = 14.0;
pub(crate) const LINE_WIDTH: f32 = 2.0;
pub(crate) const LINE_MARKER_SIZE: f32 = 4.0;
pub(crate) const HOVER_MARKER_SIZE: f32 = 9.0;
pub(crate) const TOOLTIP_GAP: f32 = 10.0;
pub(crate) const CROSSHAIR_DASH: f32 = 4.0;
pub(crate) const CROSSHAIR_PERIOD: f32 = 7.0;
pub(crate) const LEGEND_SWATCH_GAP: f32 = 20.0;
pub(crate) const LEGEND_ENTRY_GAP: f32 = 18.0;
pub(crate) const LEGEND_OVERFLOW_INSET: f32 = 4.0;
const TOOLTIP_RADIUS: f32 = 4.0;
const TOOLTIP_PADDING: f32 = 4.0;
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

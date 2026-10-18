//! The series roles one statistics chart names, and the fixed geometry it draws
//! them with.
//!
//! No colour is defined here: every one a frame paints is read from the active
//! style, because a chart is part of the window and a pane that kept its own
//! palette would stop matching the pane beside it the moment the style changed.
//! What a chart owns is the *role* each series plays — which the legend, the
//! tooltip and the line all name together — and the sizes a label is drawn at.

use base_ui::style::components::ChartColors;
use gpui_kit::{AbsoluteLength, Hsla, Pixels, Size, Window, rems, size};
use smol_str::SmolStr;

/// Which series a colour belongs to.
///
/// Naming the role rather than the colour is what keeps a legend, a tooltip and a
/// line from disagreeing: every one of them that names a series asks that role for
/// its colour. A tooltip entry may name none — a chart whose tooltip is a reading
/// rather than a legend — and is then drawn in the tooltip's own foreground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Series {
    /// An open-interest amount, measured in the base asset.
    Amount,
    /// An open-interest value, measured in the quote asset.
    Value,
    /// The long side of a ratio, and the volume that bought.
    Long,
    /// The short side of a ratio, and the volume that sold.
    Short,
    /// The ratio the two shares were derived from.
    Ratio,
}

impl Series {
    /// The colour this role is read in, resolved from the active style.
    ///
    /// The two sides of a ratio are read in the market's up and down colours rather
    /// than in two of the chart's own: which side of a long/short reading is which
    /// is the same question every other pane answers in green and red, and a chart
    /// that answered it in series colours would disagree with the pane beside it.
    ///
    /// The ratio is not a third one of those sides. It is the reading the two were
    /// derived from, drawn over them in the colour the chart reads its content in,
    /// so it takes `content` — the same value the caller resolves for every other
    /// piece of foreground in the frame — rather than a series colour of its own.
    /// A role that answered with a colour nothing is painted in would leave the
    /// legend naming a line that is not there.
    pub(crate) fn color(self, colors: &ChartColors, content: Hsla) -> Hsla {
        match self {
            Self::Amount => colors.series[0].hsla,
            Self::Value => colors.series[1].hsla,
            Self::Long => colors.bullish.hsla,
            Self::Short => colors.bearish.hsla,
            Self::Ratio => content,
        }
    }
}

/// One entry of a chart's legend, and of its tooltip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeriesLabel {
    pub series: Series,
    pub label: SmolStr,
}

/// The measured geometry of a tooltip, which is drawn on the canvas rather
/// than built as an element: it has to follow the pointer inside the plot,
/// and a popover layer would escape the clipping that keeps it there.
///
/// Drawing the box here is the one reason its geometry is spelled out at all:
/// what it reproduces is the box the framework gives `base_ui`'s own tooltip,
/// so the constants below are that tooltip's classes; the colours it is filled
/// with are `components.tooltip`'s.
pub(crate) fn tooltip_style(window: &Window) -> TooltipStyle {
    let rem_size = window.rem_size();
    let font_size = rems(TOOLTIP_FONT_SIZE_REMS).to_pixels(rem_size);
    TooltipStyle {
        font_size,
        // A line is as tall as the window's own text style makes one, which is
        // how tall the framework lays a line of this size out.
        line_height: window
            .text_style()
            .line_height
            .to_pixels(AbsoluteLength::Pixels(font_size), rem_size),
        padding: rems(TOOLTIP_PADDING_REMS).to_pixels(rem_size),
        radius: rems(TOOLTIP_RADIUS_REMS).to_pixels(rem_size),
    }
}

/// Font size of every axis, legend and time label.
pub(crate) const AXIS_FONT_SIZE: f32 = 10.0;
/// Line height of every axis, legend and time label.
pub(crate) const AXIS_LINE_HEIGHT: f32 = 14.0;
/// Width of a series line.
pub(crate) const LINE_WIDTH: f32 = 2.0;
/// The side of the square drawn at every point of a series.
pub(crate) const LINE_MARKER_SIZE: f32 = 4.0;
/// The side of the square drawn at the point under the pointer.
pub(crate) const HOVER_MARKER_SIZE: f32 = 9.0;
/// The gap a tooltip keeps from the pointer.
pub(crate) const TOOLTIP_GAP: f32 = 10.0;
/// How long one crosshair dash is, and how far apart two of them are.
pub(crate) const CROSSHAIR_DASH: f32 = 4.0;
pub(crate) const CROSSHAIR_PERIOD: f32 = 7.0;
/// The gap between a legend's swatch and its label.
pub(crate) const LEGEND_SWATCH_GAP: f32 = 20.0;
/// The gap between two legend entries.
pub(crate) const LEGEND_ENTRY_GAP: f32 = 18.0;
/// How far a legend that does not fit is kept from the edge it starts at.
pub(crate) const LEGEND_OVERFLOW_INSET: f32 = 4.0;
/// The corner of a tooltip: the framework's `rounded_sm`.
pub(crate) const TOOLTIP_RADIUS_REMS: f32 = 0.25;
/// How far a tooltip's text is kept from its edges: the framework's `p_1`.
pub(crate) const TOOLTIP_PADDING_REMS: f32 = 0.25;
/// The size of a tooltip line: the framework's `text_xs`.
pub(crate) const TOOLTIP_FONT_SIZE_REMS: f32 = 0.75;

/// The measured geometry of a tooltip.
#[derive(Clone, Copy)]
pub(crate) struct TooltipStyle {
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub padding: Pixels,
    pub radius: Pixels,
}

impl TooltipStyle {
    /// The box a tooltip of `content_width` and `line_count` lines needs.
    ///
    /// It is exactly its lines with the tooltip's own padding around them: every
    /// edge keeps the text away from it, top and bottom included.
    pub(crate) fn content_size(self, content_width: Pixels, line_count: usize) -> Size<Pixels> {
        size(
            content_width + self.padding * 2.0,
            self.line_height * line_count as f32 + self.padding * 2.0,
        )
    }
}

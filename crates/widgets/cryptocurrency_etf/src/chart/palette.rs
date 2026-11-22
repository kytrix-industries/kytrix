//! The fixed geometry one ETF chart paints with, and the two style-derived
//! decisions a frame makes.
//!
//! Every colour is read from the active theme where it is used rather than kept
//! here: a chart is part of the window, and a pane that kept its own palette would
//! stop matching the pane beside it the moment the appearance changed.
//!
//! There is no background colour. The canvas has none, and a chart that filled its
//! box would paint over the rounded corners of the pane body behind it — a pane
//! body is the background, and a chart's palette must not carry a colour the window
//! already owns.

use base_ui::theme::{components::ComponentTokens, resolved::Theme};
use ui_core::{math::Size, paint::color::Color};

/// Font size of every axis and time label.
pub(crate) const AXIS_FONT_SIZE: f32 = 10.0;
/// Line height of every axis and time label.
pub(crate) const AXIS_LINE_HEIGHT: f32 = 14.0;
/// Width of the net-assets line.
pub(crate) const LINE_WIDTH: f32 = 2.0;
/// The gap a tooltip keeps from the pointer.
pub(crate) const TOOLTIP_GAP: f32 = 10.0;
/// How long one crosshair dash is, and how far apart two of them are.
pub(crate) const CROSSHAIR_DASH: f32 = 4.0;
pub(crate) const CROSSHAIR_PERIOD: f32 = 7.0;
/// The radius of a tooltip surface: the framework's `rounded_sm`.
const TOOLTIP_RADIUS: f32 = 4.0;
/// How far a tooltip's text is kept from its edges: the framework's `p_1`.
const TOOLTIP_PADDING: f32 = 4.0;
/// A tooltip line is as tall as its own text is laid out: the compact ladder's line
/// box, which is what the framework's own tooltip draws one line in.
const TOOLTIP_LINE_HEIGHT_RATIO: f32 = 1.4;

/// The colour a day's flow is drawn in, once the caller has said what a day
/// without a direction is read in.
///
/// A flow that is not a reading — a day no fund reported, or a number the service
/// could not have published — is drawn in `muted` rather than in either direction:
/// colouring it would claim a direction it does not have. The muted colour is the
/// caller's because a flow is drawn on the plot, in the row of readings and inside a
/// tooltip, and each of those reads neutral text in the colour of its own surface.
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

/// The measured geometry of a tooltip, which is drawn on the canvas rather than
/// built as an element: it has to follow the pointer inside the plot, and a popover
/// layer would escape the clipping that keeps it there.
///
/// Drawing the box here is the one reason its geometry is spelled out at all: what it
/// reproduces is the box the framework gives `base_ui`'s own tooltip, so the
/// constants below are that tooltip's classes; the colours it is filled with are
/// `components.tooltip`'s.
pub(crate) fn tooltip_style(tokens: &ComponentTokens) -> TooltipStyle {
    let font_size = tokens.small.font_size;
    TooltipStyle {
        font_size,
        line_height: font_size * TOOLTIP_LINE_HEIGHT_RATIO,
        padding: TOOLTIP_PADDING,
        radius: TOOLTIP_RADIUS,
    }
}

/// The measured geometry of a tooltip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TooltipStyle {
    pub font_size: f32,
    pub line_height: f32,
    pub padding: f32,
    pub radius: f32,
}

impl TooltipStyle {
    /// The box a tooltip of `content_width` and `line_count` lines needs.
    ///
    /// It is exactly its lines with the tooltip's own padding around them: every
    /// edge keeps the text away from it, top and bottom included.
    pub(crate) fn content_size(self, content_width: f32, line_count: usize) -> Size {
        Size::new(
            content_width + self.padding * 2.0,
            self.line_height * line_count as f32 + self.padding * 2.0,
        )
    }
}

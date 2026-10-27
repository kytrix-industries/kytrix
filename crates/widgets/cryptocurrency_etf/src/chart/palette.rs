//! The fixed geometry one ETF chart paints with, and the two style-derived
//! decisions a frame makes.
//!
//! Every colour is read from the active style where it is used rather than kept
//! here: a chart is part of the window, and a pane that kept its own palette would
//! stop matching the pane beside it the moment the style changed.
//!
//! There is no background colour. The canvas has none, and a chart that filled its
//! box would paint over the rounded corners of the pane body behind it — a pane
//! body is the background, and a chart's palette must not carry a colour the window
//! already owns.

use base_ui::style::theme::Theme;
use gpui_kit::{AbsoluteLength, Hsla, Pixels, Size, Window, px, rems, size};

/// Font size of every axis and time label.
pub(crate) const AXIS_FONT_SIZE: Pixels = px(10.0);
/// Line height of every axis and time label.
pub(crate) const AXIS_LINE_HEIGHT: Pixels = px(14.0);
/// Width of the net-assets line.
pub(crate) const LINE_WIDTH: Pixels = px(2.0);
/// The gap a tooltip keeps from the pointer.
pub(crate) const TOOLTIP_GAP: Pixels = px(10.0);
/// How long one crosshair dash is, and how far apart two of them are.
pub(crate) const CROSSHAIR_DASH: Pixels = px(4.0);
pub(crate) const CROSSHAIR_PERIOD: Pixels = px(7.0);
/// The radius of a tooltip surface: the framework's `rounded_sm`.
const TOOLTIP_RADIUS_REMS: f32 = 0.25;
/// How far a tooltip's text is kept from its edges: the framework's `p_1`.
const TOOLTIP_PADDING_REMS: f32 = 0.25;
/// The size of a tooltip line: the framework's `text_xs`.
const TOOLTIP_FONT_SIZE_REMS: f32 = 0.75;

/// The colour a day's flow is drawn in, once the caller has said what a day
/// without a direction is read in.
///
/// A flow that is not a reading — a day no fund reported, or a number the
/// service could not have published — is drawn in `muted` rather than in either
/// direction: colouring it would claim a direction it does not have. The muted
/// colour is the caller's because a flow is drawn on the plot, in the row of
/// readings and inside a tooltip, and each of those reads neutral text in the
/// colour of its own surface.
pub(crate) fn flow(theme: &Theme, value: f64, muted: Hsla) -> Hsla {
    let chart = &theme.components.chart;
    if !value.is_finite() {
        muted
    } else if value < 0.0 {
        chart.bearish.hsla
    } else if value > 0.0 {
        chart.bullish.hsla
    } else {
        muted
    }
}

/// The measured geometry of a tooltip, which is drawn on the canvas rather
/// than built as an element: it has to follow the pointer inside the plot, and
/// a popover layer would escape the clipping that keeps it there.
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

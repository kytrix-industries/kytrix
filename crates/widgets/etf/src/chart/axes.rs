//! Where everything on an ETF chart goes, and what its axes cover.
//!
//! The chart is five boxes resolved from one: a plot, a y-axis on each side of it,
//! a time axis below, and the whole content box. Nothing here paints, and nothing
//! here talks to a window: every box is a function of the element's own bounds and
//! of the labels the axes will show, so a resize needs no state and the same inputs
//! always produce the same layout.
//!
//! The y-axes are not a fixed width. Each has to fit the widest label its scale can
//! produce, and widening an axis takes width away from the plot — so the plot keeps
//! a minimum and the *content* grows instead of the labels being squeezed. That
//! growth is what the pane scrolls.
//!
//! The two domains are the two readings a chart of ETF flow draws, and they differ
//! in the one way that matters: a flow is read *as a share of the day it belongs
//! to*, so its zero is the middle of the plot and both of its sides have to fit;
//! net assets are a level, and a window of levels often sits far from zero, so the
//! axis follows the window instead of anchoring at a baseline none of the readings
//! is near.

use gpui_kit::{Bounds, Pixels, Point, Size, point, px, size};
use smol_str::SmolStr;

use super::palette::AXIS_LINE_HEIGHT;

/// Narrowest plot, which a wide label may not squeeze further.
pub(crate) const MINIMUM_PLOT_WIDTH: Pixels = px(48.0);
/// Narrowest y-axis, used when the labels are short.
pub(crate) const MINIMUM_AXIS_WIDTH: Pixels = px(42.0);
/// The room a label keeps inside its axis.
pub(crate) const AXIS_LABEL_HORIZONTAL_PADDING: Pixels = px(12.0);
/// How far a tick label is inset from the plot's own edge.
pub(crate) const AXIS_TICK_TEXT_INSET: f32 = 8.0;
/// The smallest chart worth drawing, and the size the content box grows to when
/// the pane is narrower or shorter than this.
pub(crate) const CHART_MIN_CONTENT_WIDTH: Pixels = px(640.0);
pub(crate) const CHART_MIN_CONTENT_HEIGHT: Pixels = px(360.0);
/// How far the first and last point are kept from the plot's edges.
pub(crate) const DATA_EDGE_INSET: Pixels = px(12.0);
/// The room the time axis keeps under the plot.
pub(crate) const TIME_AXIS_HEIGHT: Pixels = px(28.0);
/// The room kept above the plot for the two axes' titles.
pub(crate) const TOP_INSET: Pixels = px(22.0);
/// The most grid lines an axis draws.
pub(crate) const MAX_AXIS_TICKS: usize = 8;
/// The most time labels a chart draws, whatever its width.
pub(crate) const MAX_TIME_TICKS: usize = 6;
/// The room one time label is given, which is also how many of them fit.
pub(crate) const TIME_TICK_WIDTH: Pixels = px(108.0);

/// The widest a flow bar is drawn.
const MAXIMUM_BAR_WIDTH: Pixels = px(14.0);
/// The share of its slot a flow bar fills.
const BAR_WIDTH_RATIO: f32 = 0.70;
/// How much room a flow axis keeps beyond its most extreme reading, as a share of
/// the spread between them.
const FLOW_HEADROOM_RATIO: f64 = 0.08;
/// The most room a net-asset axis keeps beyond its window, as a share of it.
const NET_ASSETS_HEADROOM_RATIO: f64 = 0.05;
/// Include zero when it is no more than this many spreads away from the window.
const NET_ASSETS_ZERO_ANCHOR_MAX_SPREADS: f64 = 1.0;
/// The side of the square drawn at the net-assets reading under the pointer.
pub(crate) const HOVER_MARKER_SIZE: Pixels = px(9.0);

/// The box a chart is drawn in, and the axes around it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartLayout {
    /// The content box, which may be wider or taller than the pane.
    pub content: Bounds<Pixels>,
    /// The axis the flow is read against, which is absent when a fund with no
    /// flow leaves nothing to read.
    pub left_axis: Bounds<Pixels>,
    pub plot: Bounds<Pixels>,
    /// The axis the net assets are read against, which is absent while a single
    /// fund is shown — one fund has a flow but no net assets of its own.
    pub right_axis: Bounds<Pixels>,
    pub time_axis: Bounds<Pixels>,
}

impl ChartLayout {
    /// Places every box inside `bounds`, reserving room for the labels.
    ///
    /// The widths are measured label widths rather than final ones: a label is never
    /// squeezed to fit a narrow pane, because a truncated axis label is a reading
    /// nobody can use. The *content* grows past the pane instead, and the pane
    /// scrolls to it.
    pub(crate) fn new(
        bounds: Bounds<Pixels>,
        left_label_width: Pixels,
        right_label_width: Option<Pixels>,
    ) -> Self {
        let left_width = axis_width(left_label_width);
        let right_width = right_label_width.map_or(Pixels::ZERO, axis_width);
        let content_width = bounds
            .size
            .width
            .max(MINIMUM_PLOT_WIDTH + left_width + right_width)
            .max(CHART_MIN_CONTENT_WIDTH);
        let content_height = bounds.size.height.max(CHART_MIN_CONTENT_HEIGHT);
        let content = Bounds::new(bounds.origin, size(content_width, content_height));
        let time_height = TIME_AXIS_HEIGHT.min(content.size.height * 0.12);
        let top_inset = TOP_INSET.min((content.size.height - time_height - px(1.0)).max(px(0.0)));
        let plot_width = (content.size.width - left_width - right_width).max(px(1.0));
        let plot_height = (content.size.height - top_inset - time_height).max(px(1.0));
        let plot = Bounds::new(
            point(content.origin.x + left_width, content.origin.y + top_inset),
            size(plot_width, plot_height),
        );
        Self {
            content,
            left_axis: Bounds::new(
                point(content.origin.x, plot.origin.y),
                size(left_width, plot_height),
            ),
            plot,
            right_axis: Bounds::new(
                point(plot.right(), plot.origin.y),
                size(right_width, plot_height),
            ),
            time_axis: Bounds::new(
                point(plot.origin.x, plot.bottom()),
                size(plot_width, time_height),
            ),
        }
    }
}

/// The width an axis needs for `longest_label`.
pub(crate) fn axis_width(longest_label: Pixels) -> Pixels {
    (longest_label.ceil() + AXIS_LABEL_HORIZONTAL_PADDING).max(MINIMUM_AXIS_WIDTH)
}

/// The content box one chart needs, which is the box its pane scrolls to.
///
/// It is the same arithmetic the layout applies, asked ahead of a frame: a pane that
/// is narrower than its axes need has to know that before it draws, because the width
/// it reports is what the pane scrolls to.
///
/// It is a function of the labels and nothing else. The box the last frame was laid
/// out in is deliberately not part of it — a minimum that took the pane's own size
/// into it could only ever grow, and a pane that shrank would keep drawing at the
/// width it had when it was last enlarged.
pub(crate) fn content_min_size(
    left_label_width: Pixels,
    right_label_width: Option<Pixels>,
) -> Size<Pixels> {
    let left = axis_width(left_label_width);
    let right = right_label_width.map_or(Pixels::ZERO, axis_width);
    size(
        (left + right + MINIMUM_PLOT_WIDTH).max(CHART_MIN_CONTENT_WIDTH),
        CHART_MIN_CONTENT_HEIGHT,
    )
}

/// The values an axis covers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ValueDomain {
    pub minimum: f64,
    pub maximum: f64,
}

impl ValueDomain {
    pub(crate) const fn new(minimum: f64, maximum: f64) -> Self {
        Self { minimum, maximum }
    }

    pub(crate) fn range(self) -> f64 {
        self.maximum - self.minimum
    }
}

/// A domain no reading can be plotted against, used where a window has no finite
/// values at all.
const DEGENERATE_DOMAIN: ValueDomain = ValueDomain::new(-1.0, 1.0);

/// A domain for a flow series, which is read as a share of a whole.
///
/// Zero is always inside it: a bar is drawn from the baseline, and a window of
/// inflows whose axis started at the smallest one would draw every bar as a full
/// bar. Both ends keep a little room, because a bar that touched the top of the
/// plot would read as a bar that was cut off.
pub(crate) fn flow_domain(values: impl Iterator<Item = f64>) -> ValueDomain {
    let mut minimum: f64 = 0.0;
    let mut maximum: f64 = 0.0;
    for value in values.filter(|value| value.is_finite()) {
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    let spread = maximum - minimum;
    if !spread.is_finite() {
        return DEGENERATE_DOMAIN;
    }
    // A window with nothing in it — no days, or every day reporting nothing — has
    // no reading to scale, so it gets a symmetric axis rather than a flat one.
    if minimum == 0.0 && maximum == 0.0 {
        return DEGENERATE_DOMAIN;
    }
    let range = spread.max(f64::EPSILON);
    if minimum < 0.0 {
        minimum -= range * FLOW_HEADROOM_RATIO;
    }
    if maximum > 0.0 {
        maximum += range * FLOW_HEADROOM_RATIO;
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return DEGENERATE_DOMAIN;
    }
    ValueDomain::new(minimum, maximum)
}

/// A domain for a series of levels, which is read against itself.
///
/// The window is what the axis follows: net assets of fifty billion that moved by
/// a hundred million would be a flat line on an axis anchored at zero. Two cases
/// keep their anchor anyway, because the level *is* the reading there: a window
/// that starts near zero, and a window that crosses it.
pub(crate) fn net_assets_domain(values: impl Iterator<Item = f64>) -> ValueDomain {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for value in values.filter(|value| value.is_finite()) {
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return ValueDomain::new(0.0, 1.0);
    }
    let spread = maximum - minimum;
    if !spread.is_finite() {
        return DEGENERATE_DOMAIN;
    }
    // A window that does not move still needs an axis with room in it, or the line
    // would be drawn through the middle of a zero-height plot.
    if spread.abs() <= f64::EPSILON {
        let padding = maximum.abs().max(1.0) * NET_ASSETS_HEADROOM_RATIO;
        let domain = ValueDomain::new(minimum - padding, maximum + padding);
        return if domain.minimum.is_finite() && domain.maximum.is_finite() {
            domain
        } else {
            DEGENERATE_DOMAIN
        };
    }
    let padding = spread * NET_ASSETS_HEADROOM_RATIO;
    let crosses_zero = minimum < 0.0 && maximum > 0.0;
    // A crossing window is padded on each side by its own magnitude, so the axis
    // keeps the proportions of the two sides the zero line divides it into.
    let minimum_padding = if crosses_zero {
        minimum.abs() * NET_ASSETS_HEADROOM_RATIO
    } else {
        padding
    };
    let maximum_padding = if crosses_zero {
        maximum.abs() * NET_ASSETS_HEADROOM_RATIO
    } else {
        padding
    };
    if !minimum_padding.is_finite() || !maximum_padding.is_finite() {
        return DEGENERATE_DOMAIN;
    }
    let minimum = if minimum >= 0.0 && minimum <= spread * NET_ASSETS_ZERO_ANCHOR_MAX_SPREADS {
        0.0
    } else {
        minimum - minimum_padding
    };
    let maximum = if maximum <= 0.0 && maximum.abs() <= spread * NET_ASSETS_ZERO_ANCHOR_MAX_SPREADS
    {
        0.0
    } else {
        maximum + maximum_padding
    };
    if !minimum.is_finite() || !maximum.is_finite() || minimum >= maximum {
        return DEGENERATE_DOMAIN;
    }
    ValueDomain::new(minimum, maximum)
}

/// One labelled position on a y-axis.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AxisTick {
    pub value: f64,
    pub label: SmolStr,
}

/// A y-axis: the values it covers and the labels it shows.
///
/// The labels are built when a snapshot arrives and live here rather than being
/// formatted per frame: an axis that re-derived its text on every paint would
/// format a dozen numbers per frame for a chart nobody is touching.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AxisScale {
    pub domain: ValueDomain,
    pub ticks: Box<[AxisTick]>,
}

impl AxisScale {
    /// Builds a scale from a domain, formatting each tick with `format`.
    pub(crate) fn new(domain: ValueDomain, format: impl Fn(f64, f64) -> SmolStr) -> Self {
        let values = nice_ticks(domain);
        let step = tick_step(&values, domain);
        let ticks = values
            .into_iter()
            .map(|value| AxisTick {
                value,
                label: format(value, step),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { domain, ticks }
    }
}

/// The values an axis draws grid lines and labels at.
///
/// The step is a seventh of the range, which is what makes an axis of at most eight
/// lines: fewer lines than that and a reader interpolating between them is guessing,
/// more and the labels start to crowd the plot they describe.
pub(crate) fn nice_ticks(domain: ValueDomain) -> Vec<f64> {
    let range = domain.range();
    if !range.is_finite() || range <= 0.0 {
        return vec![domain.minimum];
    }
    let divisions = MAX_AXIS_TICKS.saturating_sub(1).max(2) as f64;
    let step = nice_step(range / divisions);
    let tolerance = step * 1e-9;
    let mut value = (domain.minimum / step).ceil() * step;
    let mut ticks = Vec::with_capacity(MAX_AXIS_TICKS);
    while value <= domain.maximum + tolerance && ticks.len() < MAX_AXIS_TICKS {
        // A value that accumulated a negative zero is stored as a positive one:
        // the label would read "-0" otherwise, which is not a reading of anything.
        ticks.push(if value.abs() <= tolerance { 0.0 } else { value });
        let next = value + step;
        if !next.is_finite() || next <= value {
            break;
        }
        value = next;
    }
    if ticks.is_empty() {
        let midpoint = domain.minimum + range / 2.0;
        ticks.push(if midpoint.is_finite() { midpoint } else { 0.0 });
    }
    ticks
}

/// The distance between two ticks, which is what tells a label how much precision
/// it needs to stay distinct from its neighbour.
pub(crate) fn tick_step(ticks: &[f64], domain: ValueDomain) -> f64 {
    ticks
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .find(|step| step.is_finite() && *step > 0.0)
        .unwrap_or_else(|| domain.range().abs())
}

/// Rounds a raw step up to the nearest 1, 2 or 5 times a power of ten.
pub(crate) fn nice_step(raw: f64) -> f64 {
    if !raw.is_finite() || raw <= 0.0 {
        return 1.0;
    }
    let magnitude = 10_f64.powf(raw.log10().floor());
    let fraction = raw / magnitude;
    let nice_fraction = if fraction <= 1.0 {
        1.0
    } else if fraction <= 2.0 {
        2.0
    } else if fraction <= 5.0 {
        5.0
    } else {
        10.0
    };
    (nice_fraction * magnitude).max(f64::EPSILON)
}

/// The y coordinate of `value` inside `plot`.
pub(crate) fn value_y(value: f64, domain: ValueDomain, plot: Bounds<Pixels>) -> Pixels {
    let range = domain.range();
    if !range.is_finite() || range <= f64::EPSILON || !value.is_finite() {
        return plot.origin.y + plot.size.height / 2.0;
    }
    let ratio = ((value - domain.minimum) / range).clamp(0.0, 1.0) as f32;
    plot.bottom() - plot.size.height * ratio
}

/// The x range the points of a series occupy, inset from the plot's edges.
pub(crate) fn data_x_range(plot: Bounds<Pixels>) -> (Pixels, Pixels) {
    let inset = DATA_EDGE_INSET.min(plot.size.width / 2.0);
    (plot.origin.x + inset, plot.right() - inset)
}

/// The x coordinate of the point at `index`, out of `point_count`.
///
/// Points are evenly spaced because the funds report one row per trading day: the
/// time axis carries the dates, so the spacing does not have to, and a week with a
/// holiday in it does not leave a gap nobody can read.
pub(crate) fn point_x(plot: Bounds<Pixels>, point_count: usize, index: usize) -> Pixels {
    if point_count <= 1 {
        return plot.origin.x + plot.size.width / 2.0;
    }
    let (left, right) = data_x_range(plot);
    let fraction = index.min(point_count - 1) as f32 / (point_count - 1) as f32;
    left + (right - left) * fraction
}

/// The index of the point nearest `x`, or `None` when there are no points.
pub(crate) fn nearest_index(plot: Bounds<Pixels>, point_count: usize, x: Pixels) -> Option<usize> {
    if point_count == 0 || plot.size.width <= Pixels::ZERO {
        return None;
    }
    if point_count == 1 {
        return Some(0);
    }
    let (left, right) = data_x_range(plot);
    let width = f32::from(right - left);
    if width <= 0.0 {
        return Some(0);
    }
    let fraction = (f32::from(x - left) / width).clamp(0.0, 1.0);
    Some((fraction * (point_count - 1) as f32).round() as usize)
}

/// The top edge of the label for a tick at `y`, centred on it.
///
/// A tick's label belongs beside its own grid line, so it is centred on the value
/// it names rather than hung below it. The first and last ticks would otherwise
/// half stick out of the axis, so they are pushed back inside it.
pub(crate) fn axis_label_y(y: Pixels, axis: Bounds<Pixels>) -> Pixels {
    (y - AXIS_LINE_HEIGHT / 2.0).clamp(
        axis.origin.y,
        (axis.bottom() - AXIS_LINE_HEIGHT).max(axis.origin.y),
    )
}

/// The bar one day's flow is drawn as.
///
/// The bar is measured from the zero line rather than from the axis floor: a flow
/// is a direction as well as a size, and a bar that grew downwards from its own
/// top would say the opposite of what it measured.
pub(crate) fn flow_bar_bounds(
    plot: Bounds<Pixels>,
    point_count: usize,
    index: usize,
    value: f64,
    domain: ValueDomain,
) -> Bounds<Pixels> {
    let slot = plot.size.width / point_count as f32;
    let width = (slot * BAR_WIDTH_RATIO)
        .min(MAXIMUM_BAR_WIDTH)
        .max(px(1.0).min(slot));
    let center = point_x(plot, point_count, index);
    let baseline = value_y(0.0, domain, plot);
    let value_y = value_y(value, domain, plot);
    let top = baseline.min(value_y);
    let height = (baseline - value_y).abs().max(px(1.0));
    Bounds::new(point(center - width / 2.0, top), size(width, height))
}

/// One position on the time axis, already placed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PositionedTimeTick {
    pub label: SmolStr,
    pub origin_x: Pixels,
    pub width: Pixels,
}

/// Places the time labels a chart has room for, dropping every other one until the
/// rest fit.
///
/// Both ends are always labelled — a time axis that stops at an arbitrary day is
/// one nobody can read a span off — so the candidates are the even divisions of the
/// window and the search is over how many of them there are.
pub(crate) fn position_time_ticks(
    labels: &[SmolStr],
    layout: ChartLayout,
) -> Box<[PositionedTimeTick]> {
    if labels.is_empty() {
        return Box::default();
    }
    let fitting = (f32::from(layout.time_axis.size.width) / f32::from(TIME_TICK_WIDTH)).floor();
    let maximum = (fitting as usize)
        .clamp(1, MAX_TIME_TICKS)
        .min(labels.len());
    for count in (1..=maximum).rev() {
        let ticks = positioned_time_ticks(labels, layout, count);
        if time_ticks_are_disjoint(&ticks) {
            return ticks;
        }
    }
    Box::default()
}

/// The evenly divided time labels of one candidate count, already placed.
fn positioned_time_ticks(
    labels: &[SmolStr],
    layout: ChartLayout,
    count: usize,
) -> Box<[PositionedTimeTick]> {
    let width = TIME_TICK_WIDTH.min(layout.time_axis.size.width);
    let maximum_x = (layout.time_axis.right() - width).max(layout.time_axis.origin.x);
    let last = labels.len() - 1;
    let divisor = count.saturating_sub(1);
    (0..count)
        .map(|position| {
            let index = if divisor == 0 {
                0
            } else {
                position
                    .checked_div(divisor)
                    .map_or(0, |share| share * last)
            };
            let center = point_x(layout.plot, labels.len(), index);
            PositionedTimeTick {
                label: labels[index].clone(),
                origin_x: (center - width / 2.0).clamp(layout.time_axis.origin.x, maximum_x),
                width,
            }
        })
        .collect()
}

/// Whether every placed time label has room of its own.
fn time_ticks_are_disjoint(ticks: &[PositionedTimeTick]) -> bool {
    let mut previous_right: Option<Pixels> = None;
    for tick in ticks {
        if previous_right.is_some_and(|right| tick.origin_x < right) {
            return false;
        }
        previous_right = Some(tick.origin_x + tick.width);
    }
    true
}

/// Where a tooltip of `desired_size` fits next to `anchor`, inside `container`.
///
/// A tooltip that followed the pointer off the plot would be clipped away exactly
/// when it is most wanted, so it flips to the other side of the pointer instead of
/// being cut in half, and stays centred on the reading it belongs to.
pub(crate) fn tooltip_bounds(
    anchor: Point<Pixels>,
    desired_size: Size<Pixels>,
    container: Bounds<Pixels>,
    gap: Pixels,
) -> Bounds<Pixels> {
    let fitted = size(
        desired_size
            .width
            .min(container.size.width.max(Pixels::ZERO)),
        desired_size
            .height
            .min(container.size.height.max(Pixels::ZERO)),
    );
    let right_x = anchor.x + gap;
    let left_x = anchor.x - fitted.width - gap;
    let x = if right_x + fitted.width <= container.right() {
        right_x
    } else {
        left_x
    };
    let maximum_x = (container.right() - fitted.width).max(container.origin.x);
    let maximum_y = (container.bottom() - fitted.height).max(container.origin.y);
    Bounds::new(
        point(
            x.clamp(container.origin.x, maximum_x),
            (anchor.y - fitted.height / 2.0).clamp(container.origin.y, maximum_y),
        ),
        fitted,
    )
}

/// The box of the square drawn at the reading under the pointer.
pub(crate) fn marker_bounds(center: Point<Pixels>) -> Bounds<Pixels> {
    let radius = HOVER_MARKER_SIZE / 2.0;
    Bounds::new(
        center - point(radius, radius),
        size(radius * 2.0, radius * 2.0),
    )
}

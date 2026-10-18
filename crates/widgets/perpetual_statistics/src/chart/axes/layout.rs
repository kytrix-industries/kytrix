//! Where everything on a statistics chart goes.
//!
//! The chart is six boxes resolved from one: a plot, a y-axis on each side of it,
//! a time axis below, a legend under that, and the whole content box. Nothing here
//! paints, and nothing here talks to a window: every box is a function of the
//! element's own bounds and of the labels the axes will show, so a resize needs no
//! state and the same inputs always produce the same layout.
//!
//! The y-axes are not a fixed width. Each has to fit the widest label its scale
//! can produce, and widening an axis takes width away from the plot — so the plot
//! keeps a minimum and the *content* grows instead of the labels being squeezed.
//! That growth is what the pane scrolls.

use gpui_kit::{Bounds, Pixels, Size, point, px, size};
use smol_str::SmolStr;

use super::super::palette::AXIS_LINE_HEIGHT;

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
pub(crate) const CHART_MIN_CONTENT_WIDTH: Pixels = px(520.0);
pub(crate) const CHART_MIN_CONTENT_HEIGHT: Pixels = px(280.0);
/// How far the first and last point are kept from the plot's edges.
pub(crate) const DATA_EDGE_INSET: Pixels = px(12.0);
pub(crate) const TIME_AXIS_HEIGHT: Pixels = px(28.0);
pub(crate) const LEGEND_HEIGHT: Pixels = px(30.0);
pub(crate) const TOP_INSET: Pixels = px(22.0);
/// The most grid lines an axis draws.
pub(crate) const MAX_AXIS_TICKS: usize = 8;
/// The most time labels a chart draws, whatever its width.
pub(crate) const MAX_TIME_TICKS: usize = 6;
/// The room one time label is given, which is also how many of them fit.
pub(crate) const TIME_TICK_WIDTH: Pixels = px(104.0);

/// The box a series is plotted in, and the axes around it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartLayout {
    /// The content box, which may be wider or taller than the pane.
    pub content: Bounds<Pixels>,
    pub left_axis: Bounds<Pixels>,
    pub plot: Bounds<Pixels>,
    pub right_axis: Bounds<Pixels>,
    pub time_axis: Bounds<Pixels>,
    pub legend: Bounds<Pixels>,
}

impl ChartLayout {
    /// Places every box inside `bounds`, reserving room for the labels.
    ///
    /// `left_label_width` is absent when the chart has no left axis at all, which
    /// is how a single-series chart keeps that width for its plot.
    pub(crate) fn new(
        bounds: Bounds<Pixels>,
        left_label_width: Option<Pixels>,
        right_label_width: Pixels,
    ) -> Self {
        let left_width =
            left_label_width.map_or(Pixels::ZERO, |width| axis_width(bounds.size.width, width));
        let right_width = axis_width(bounds.size.width, right_label_width);
        let content_width = bounds
            .size
            .width
            .max(MINIMUM_PLOT_WIDTH + left_width + right_width)
            .max(CHART_MIN_CONTENT_WIDTH);
        let content_height = bounds.size.height.max(CHART_MIN_CONTENT_HEIGHT);
        let content = Bounds::new(bounds.origin, size(content_width, content_height));
        let time_height = TIME_AXIS_HEIGHT.min(content.size.height * 0.12);
        let legend_height = LEGEND_HEIGHT.min(content.size.height * 0.14);
        let top_inset = TOP_INSET
            .min((content.size.height - time_height - legend_height - px(1.0)).max(Pixels::ZERO));
        let plot_width = (content.size.width - left_width - right_width).max(px(1.0));
        let plot_height =
            (content.size.height - top_inset - time_height - legend_height).max(px(1.0));
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
            legend: Bounds::new(
                point(content.origin.x, plot.bottom() + time_height),
                size(content.size.width, legend_height),
            ),
        }
    }
}

/// The width an axis needs for `longest_label`, never squeezing the plot below
/// its minimum and never claiming more than the chart has.
pub(crate) fn axis_width(chart_width: Pixels, longest_label: Pixels) -> Pixels {
    let desired = (longest_label.ceil() + AXIS_LABEL_HORIZONTAL_PADDING).max(MINIMUM_AXIS_WIDTH);
    let available = (chart_width - MINIMUM_PLOT_WIDTH).max(Pixels::ZERO);
    desired.min(available)
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

/// A domain that grows to fit a line, with a little room above and below it.
///
/// A line that touched the top of the plot would read as a line that was cut off.
/// Values that are not readings of anything — negative, infinite, not a number —
/// are left out rather than allowed to stretch the axis to infinity.
pub(crate) fn line_domain(values: impl Iterator<Item = f64>) -> ValueDomain {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for value in values.filter(|value| value.is_finite() && *value >= 0.0) {
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    if !minimum.is_finite() || !maximum.is_finite() || maximum == 0.0 {
        return ValueDomain::new(0.0, 1.0);
    }
    let range = maximum - minimum;
    let padding = if range > 0.0 {
        range * 0.06
    } else {
        (maximum.abs() * 0.06).max(1e-9)
    };
    ValueDomain::new((minimum - padding).max(0.0), maximum + padding)
}

/// A domain that starts at zero and grows to fit the tallest bar.
///
/// A bar is read as a share of its interval, so its baseline is the axis floor: a
/// volume chart that floated its baseline would make two similar intervals look
/// different by an amount nobody can measure.
pub(crate) fn bar_domain(values: impl Iterator<Item = f64>) -> ValueDomain {
    const HEADROOM: f64 = 1.1;
    let maximum = values
        .filter(|value| value.is_finite() && *value >= 0.0)
        .fold(0.0_f64, f64::max);
    ValueDomain::new(
        0.0,
        if maximum == 0.0 {
            1.0
        } else {
            maximum * HEADROOM
        },
    )
}

/// A domain that has to fit a ratio, which has no natural zero.
pub(crate) fn ratio_domain(values: impl Iterator<Item = f64>) -> ValueDomain {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for value in values.filter(|value| value.is_finite() && *value > 0.0) {
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return ValueDomain::new(0.0, 1.0);
    }
    if minimum == maximum {
        let padding = (minimum.abs() * 0.08).max(1e-9);
        return ValueDomain::new((minimum - padding).max(0.0), maximum + padding);
    }
    let padding = (maximum - minimum) * 0.08;
    let step = nice_step(((maximum + padding) - (minimum - padding).max(0.0)) / 5.0);
    let nice_minimum = ((minimum - padding).max(0.0) / step).floor() * step;
    let candidate = ((maximum + padding) / step).ceil() * step;
    let nice_maximum = if candidate.is_finite() && candidate > nice_minimum {
        candidate
    } else {
        maximum.max(nice_minimum + step)
    };
    ValueDomain::new(nice_minimum, nice_maximum)
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

/// A scale with the same ticks whatever the data says, for a share axis whose
/// whole is one by definition.
pub(crate) fn share_scale(ticks: &'static [(f64, &'static str)]) -> AxisScale {
    AxisScale {
        domain: ValueDomain::new(0.0, 1.0),
        ticks: ticks
            .iter()
            .map(|(value, label)| AxisTick {
                value: *value,
                label: SmolStr::new_static(label),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    }
}

/// One position on the time axis, already placed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PositionedTimeTick {
    pub label: SmolStr,
    pub origin_x: Pixels,
    pub width: Pixels,
}

/// The values an axis draws grid lines and labels at.
pub(crate) fn nice_ticks(domain: ValueDomain) -> Vec<f64> {
    let range = domain.range();
    if !range.is_finite() || range <= 0.0 {
        return vec![domain.minimum];
    }
    let step = nice_step(range / 5.0);
    let mut value = (domain.minimum / step).ceil() * step;
    let mut ticks = Vec::with_capacity(MAX_AXIS_TICKS);
    while value <= domain.maximum + step * 1e-9 && ticks.len() < MAX_AXIS_TICKS {
        ticks.push(value);
        let next = value + step;
        if next <= value {
            break;
        }
        value = next;
    }
    if ticks.is_empty() {
        ticks.push((domain.minimum + domain.maximum) / 2.0);
    }
    ticks
}

/// The distance between two ticks, which is what tells a compact label how much
/// precision it needs to stay distinct from its neighbour.
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
    nice_fraction * magnitude
}

/// The y coordinate of `value` inside `plot`.
pub(crate) fn value_y(value: f64, domain: ValueDomain, plot: Bounds<Pixels>) -> Pixels {
    let ratio = if domain.range() > 0.0 && value.is_finite() {
        ((value - domain.minimum) / domain.range()).clamp(0.0, 1.0)
    } else {
        0.5
    };
    plot.bottom() - plot.size.height * ratio as f32
}

/// The x range the points of a series occupy, inset from the plot's edges.
pub(crate) fn data_x_range(plot: Bounds<Pixels>) -> (Pixels, Pixels) {
    let inset = DATA_EDGE_INSET.min(plot.size.width / 2.0);
    (plot.origin.x + inset, plot.right() - inset)
}

/// The x coordinate of the point at `index`, out of `point_count`.
///
/// Points are evenly spaced because the venues bucket these series into even
/// intervals: the time axis carries the time, so the spacing does not have to.
pub(crate) fn point_x(plot: Bounds<Pixels>, point_count: usize, index: usize) -> Pixels {
    if point_count <= 1 {
        return plot.origin.x + plot.size.width / 2.0;
    }
    let (left, right) = data_x_range(plot);
    let fraction = index.min(point_count - 1) as f32 / (point_count - 1) as f32;
    left + (right - left) * fraction
}

/// The top edge of the label for a tick at `y`, centred on it.
///
/// A tick's label belongs beside its own grid line, so it is centred on the value
/// it names rather than hung below it. The first and last ticks would otherwise
/// half stick out of the axis, so they are pushed back inside it.
pub(crate) fn axis_label_y(y: Pixels, axis: Bounds<Pixels>) -> Pixels {
    (y - px(AXIS_LINE_HEIGHT) / 2.0).clamp(
        axis.origin.y,
        (axis.bottom() - px(AXIS_LINE_HEIGHT)).max(axis.origin.y),
    )
}

/// The nearest index that is one of `valid`, which is how a hover skips a point
/// the chart does not draw.
pub(crate) fn valid_index(valid: &[usize], index: usize) -> Option<usize> {
    if valid.is_empty() {
        return None;
    }
    let at = valid.partition_point(|&valid| valid < index);
    match (valid.get(at.saturating_sub(1)), valid.get(at)) {
        (Some(&left), Some(&right)) => Some(if index - left <= right - index {
            left
        } else {
            right
        }),
        (Some(&left), None) => Some(left),
        (None, Some(right)) => Some(*right),
        (None, None) => None,
    }
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

/// The two halves of one taker-volume bar.
pub(crate) fn paired_bar_bounds(
    plot: Bounds<Pixels>,
    point_count: usize,
    index: usize,
    buy: f64,
    sell: f64,
    domain: ValueDomain,
) -> (Bounds<Pixels>, Bounds<Pixels>) {
    let (buy_left, sell_left, bar_width) = paired_bar_x_bounds(plot, point_count, index);
    let baseline = value_y(0.0, domain, plot);
    let buy_top = value_y(buy, domain, plot);
    let sell_top = value_y(sell, domain, plot);
    (
        Bounds::new(
            point(buy_left, buy_top),
            size(bar_width, (baseline - buy_top).max(px(1.0))),
        ),
        Bounds::new(
            point(sell_left, sell_top),
            size(bar_width, (baseline - sell_top).max(px(1.0))),
        ),
    )
}

/// Where the buy and sell halves of one bar sit, and how wide each is.
pub(crate) fn paired_bar_x_bounds(
    plot: Bounds<Pixels>,
    point_count: usize,
    index: usize,
) -> (Pixels, Pixels, Pixels) {
    let slot_width = if point_count <= 1 {
        f32::from(plot.size.width).min(32.0)
    } else {
        f32::from(plot.size.width) / point_count as f32
    };
    let group_width = (slot_width * 0.72).clamp(4.0, 24.0);
    // The two halves touch, and the seam between them lands on the point's own x:
    // they are one interval's volume split by the side that crossed the spread, so
    // a gap would read as an empty interval between the two sides of one.
    let bar_width = px(group_width / 2.0);
    let center = point_x(plot, point_count, index);
    (center - bar_width, center, bar_width)
}

/// The x range one stacked ratio bar occupies.
pub(crate) fn ratio_bar_width(plot: Bounds<Pixels>, point_count: usize) -> Pixels {
    if point_count == 0 {
        return px(3.0);
    }
    (plot.size.width / point_count as f32 * 0.62).clamp(px(3.0), px(18.0))
}

/// The two halves of one stacked ratio bar: the long side grows up from the floor
/// and the short side comes down from the ceiling, so the bar is always a whole.
pub(crate) fn stacked_bar_bounds(
    center_x: Pixels,
    width: Pixels,
    long_share: f64,
    short_share: f64,
    plot: Bounds<Pixels>,
) -> (Bounds<Pixels>, Bounds<Pixels>) {
    let total = long_share + short_share;
    let normalized_long = if total > 0.0 {
        (long_share / total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let long_height = plot.size.height * normalized_long as f32;
    let short_height = plot.size.height - long_height;
    (
        Bounds::new(
            point(center_x - width / 2.0, plot.bottom() - long_height),
            size(width, long_height),
        ),
        Bounds::new(
            point(center_x - width / 2.0, plot.origin.y),
            size(width, short_height),
        ),
    )
}

/// The indices the time axis labels, spread over the whole window.
///
/// Both ends are always labelled, because a time axis that starts at an arbitrary
/// interval is one nobody can read a duration off.
pub(crate) fn label_indices(point_count: usize, maximum_labels: usize) -> Vec<usize> {
    if point_count == 0 || maximum_labels == 0 {
        return Vec::new();
    }
    if point_count <= maximum_labels {
        return (0..point_count).collect();
    }
    let last = point_count - 1;
    let divisor = maximum_labels - 1;
    (0..maximum_labels)
        .map(|index| (index * last + divisor / 2) / divisor)
        .fold(Vec::with_capacity(maximum_labels), |mut indices, index| {
            if indices.last() != Some(&index) {
                indices.push(index);
            }
            indices
        })
}

/// The indices the time axis labels at this plot width.
pub(crate) fn time_tick_indices(point_count: usize, plot_width: Pixels) -> Vec<usize> {
    let fitting = (f32::from(plot_width) / f32::from(TIME_TICK_WIDTH)).floor() as usize;
    label_indices(
        point_count,
        fitting.clamp(2, MAX_TIME_TICKS).min(point_count.max(1)),
    )
}

/// Places every labelled tick inside the time axis, centred on its point.
pub(crate) fn position_time_ticks(
    labels: impl IntoIterator<Item = (usize, SmolStr)>,
    point_count: usize,
    layout: ChartLayout,
) -> Box<[PositionedTimeTick]> {
    let width = TIME_TICK_WIDTH.min(layout.time_axis.size.width);
    let maximum_x = (layout.time_axis.right() - width).max(layout.time_axis.origin.x);
    labels
        .into_iter()
        .map(|(index, label)| {
            let x = point_x(layout.plot, point_count, index);
            PositionedTimeTick {
                label,
                origin_x: (x - width / 2.0).clamp(layout.time_axis.origin.x, maximum_x),
                width,
            }
        })
        .collect()
}

/// Where a tooltip of `desired_size` fits next to `anchor`, inside `container`.
///
/// A tooltip that followed the pointer off the plot would be clipped away exactly
/// when it is most wanted, so it flips to the other side of the pointer instead.
pub(crate) fn tooltip_bounds(
    anchor: gpui_kit::Point<Pixels>,
    desired_size: Size<Pixels>,
    container: Bounds<Pixels>,
    gap: Pixels,
) -> Bounds<Pixels> {
    let fitted = size(
        desired_size
            .width
            .min(container.size.width)
            .max(Pixels::ZERO),
        desired_size
            .height
            .min(container.size.height)
            .max(Pixels::ZERO),
    );
    let mut x = anchor.x + gap;
    let mut y = anchor.y + gap;
    if x + fitted.width > container.right() {
        x = anchor.x - gap - fitted.width;
    }
    if y + fitted.height > container.bottom() {
        y = anchor.y - gap - fitted.height;
    }
    let maximum_x = (container.right() - fitted.width).max(container.origin.x);
    let maximum_y = (container.bottom() - fitted.height).max(container.origin.y);
    Bounds::new(
        point(
            x.clamp(container.origin.x, maximum_x),
            y.clamp(container.origin.y, maximum_y),
        ),
        fitted,
    )
}

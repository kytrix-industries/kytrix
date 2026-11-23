use smol_str::SmolStr;
use ui_core::math::{Point, Rect, Size};

use super::{palette::AXIS_LINE_HEIGHT, rect::RectExt};

pub(crate) const MINIMUM_PLOT_WIDTH: f32 = 48.0;
pub(crate) const MINIMUM_AXIS_WIDTH: f32 = 42.0;
pub(crate) const AXIS_LABEL_HORIZONTAL_PADDING: f32 = 12.0;
pub(crate) const AXIS_TICK_TEXT_INSET: f32 = 8.0;
pub(crate) const CHART_MIN_CONTENT_WIDTH: f32 = 520.0;
pub(crate) const CHART_MIN_CONTENT_HEIGHT: f32 = 280.0;
pub(crate) const DATA_EDGE_INSET: f32 = 12.0;
pub(crate) const TIME_AXIS_HEIGHT: f32 = 28.0;
pub(crate) const LEGEND_HEIGHT: f32 = 30.0;
pub(crate) const TOP_INSET: f32 = 22.0;
pub(crate) const MAX_AXIS_TICKS: usize = 8;
pub(crate) const MAX_TIME_TICKS: usize = 6;
pub(crate) const TIME_TICK_WIDTH: f32 = 104.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartLayout {
    pub content: Rect,
    pub left_axis: Rect,
    pub plot: Rect,
    pub right_axis: Rect,
    pub time_axis: Rect,
    pub legend: Rect,
}

impl ChartLayout {
    pub(crate) fn new(bounds: Rect, left_label_width: Option<f32>, right_label_width: f32) -> Self {
        let left_width = left_label_width.map_or(0.0, |width| axis_width(bounds.size.width, width));
        let right_width = axis_width(bounds.size.width, right_label_width);
        let content_width = bounds
            .size
            .width
            .max(MINIMUM_PLOT_WIDTH + left_width + right_width)
            .max(CHART_MIN_CONTENT_WIDTH);
        let content_height = bounds.size.height.max(CHART_MIN_CONTENT_HEIGHT);
        let content =
            Rect::from_origin_size(bounds.origin, Size::new(content_width, content_height));
        let time_height = TIME_AXIS_HEIGHT.min(content.size.height * 0.12);
        let legend_height = LEGEND_HEIGHT.min(content.size.height * 0.14);
        let top_inset =
            TOP_INSET.min((content.size.height - time_height - legend_height - 1.0).max(0.0));
        let plot_width = (content.size.width - left_width - right_width).max(1.0);
        let plot_height = (content.size.height - top_inset - time_height - legend_height).max(1.0);
        let plot = Rect::from_origin_size(
            Point::new(content.origin.x + left_width, content.origin.y + top_inset),
            Size::new(plot_width, plot_height),
        );
        Self {
            content,
            left_axis: Rect::from_origin_size(
                Point::new(content.origin.x, plot.origin.y),
                Size::new(left_width, plot_height),
            ),
            plot,
            right_axis: Rect::from_origin_size(
                Point::new(plot.right(), plot.origin.y),
                Size::new(right_width, plot_height),
            ),
            time_axis: Rect::from_origin_size(
                Point::new(plot.origin.x, plot.bottom()),
                Size::new(plot_width, time_height),
            ),
            legend: Rect::from_origin_size(
                Point::new(content.origin.x, plot.bottom() + time_height),
                Size::new(content.size.width, legend_height),
            ),
        }
    }
}

pub(crate) fn axis_width(chart_width: f32, longest_label: f32) -> f32 {
    let desired = (longest_label.ceil() + AXIS_LABEL_HORIZONTAL_PADDING).max(MINIMUM_AXIS_WIDTH);
    let available = (chart_width - MINIMUM_PLOT_WIDTH).max(0.0);
    desired.min(available)
}

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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AxisTick {
    pub value: f64,
    pub label: SmolStr,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AxisScale {
    pub domain: ValueDomain,
    pub ticks: Box<[AxisTick]>,
}

impl AxisScale {
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PositionedTimeTick {
    pub label: SmolStr,
    pub origin_x: f32,
    pub width: f32,
}

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

pub(crate) fn tick_step(ticks: &[f64], domain: ValueDomain) -> f64 {
    ticks
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .find(|step| step.is_finite() && *step > 0.0)
        .unwrap_or_else(|| domain.range().abs())
}

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

pub(crate) fn value_y(value: f64, domain: ValueDomain, plot: Rect) -> f32 {
    let ratio = if domain.range() > 0.0 && value.is_finite() {
        ((value - domain.minimum) / domain.range()).clamp(0.0, 1.0)
    } else {
        0.5
    };
    plot.bottom() - plot.size.height * ratio as f32
}

pub(crate) fn data_x_range(plot: Rect) -> (f32, f32) {
    let inset = DATA_EDGE_INSET.min(plot.size.width / 2.0);
    (plot.origin.x + inset, plot.right() - inset)
}

pub(crate) fn point_x(plot: Rect, point_count: usize, index: usize) -> f32 {
    if point_count <= 1 {
        return plot.origin.x + plot.size.width / 2.0;
    }
    let (left, right) = data_x_range(plot);
    let fraction = index.min(point_count - 1) as f32 / (point_count - 1) as f32;
    left + (right - left) * fraction
}

pub(crate) fn axis_label_y(y: f32, axis: Rect) -> f32 {
    (y - AXIS_LINE_HEIGHT / 2.0).clamp(
        axis.origin.y,
        (axis.bottom() - AXIS_LINE_HEIGHT).max(axis.origin.y),
    )
}

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

pub(crate) fn nearest_index(plot: Rect, point_count: usize, x: f32) -> Option<usize> {
    if point_count == 0 || plot.size.width <= 0.0 {
        return None;
    }
    if point_count == 1 {
        return Some(0);
    }
    let (left, right) = data_x_range(plot);
    let width = right - left;
    if width <= 0.0 {
        return Some(0);
    }
    let fraction = ((x - left) / width).clamp(0.0, 1.0);
    Some((fraction * (point_count - 1) as f32).round() as usize)
}

pub(crate) fn paired_bar_bounds(
    plot: Rect,
    point_count: usize,
    index: usize,
    buy: f64,
    sell: f64,
    domain: ValueDomain,
) -> (Rect, Rect) {
    let (buy_left, sell_left, bar_width) = paired_bar_x_bounds(plot, point_count, index);
    let baseline = value_y(0.0, domain, plot);
    let buy_top = value_y(buy, domain, plot);
    let sell_top = value_y(sell, domain, plot);
    (
        Rect::from_origin_size(
            Point::new(buy_left, buy_top),
            Size::new(bar_width, (baseline - buy_top).max(1.0)),
        ),
        Rect::from_origin_size(
            Point::new(sell_left, sell_top),
            Size::new(bar_width, (baseline - sell_top).max(1.0)),
        ),
    )
}

pub(crate) fn paired_bar_x_bounds(plot: Rect, point_count: usize, index: usize) -> (f32, f32, f32) {
    let slot_width = if point_count <= 1 {
        plot.size.width.min(32.0)
    } else {
        plot.size.width / point_count as f32
    };
    let group_width = (slot_width * 0.72).clamp(4.0, 24.0);
    let bar_width = group_width / 2.0;
    let center = point_x(plot, point_count, index);
    (center - bar_width, center, bar_width)
}

pub(crate) fn ratio_bar_width(plot: Rect, point_count: usize) -> f32 {
    if point_count == 0 {
        return 3.0;
    }
    (plot.size.width / point_count as f32 * 0.62).clamp(3.0, 18.0)
}

pub(crate) fn stacked_bar_bounds(
    center_x: f32,
    width: f32,
    long_share: f64,
    short_share: f64,
    plot: Rect,
) -> (Rect, Rect) {
    let total = long_share + short_share;
    let normalized_long = if total > 0.0 {
        (long_share / total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let long_height = plot.size.height * normalized_long as f32;
    let short_height = plot.size.height - long_height;
    (
        Rect::from_origin_size(
            Point::new(center_x - width / 2.0, plot.bottom() - long_height),
            Size::new(width, long_height),
        ),
        Rect::from_origin_size(
            Point::new(center_x - width / 2.0, plot.origin.y),
            Size::new(width, short_height),
        ),
    )
}

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

pub(crate) fn time_tick_indices(point_count: usize, plot_width: f32) -> Vec<usize> {
    let fitting = (plot_width / TIME_TICK_WIDTH).floor() as usize;
    label_indices(
        point_count,
        fitting.clamp(2, MAX_TIME_TICKS).min(point_count.max(1)),
    )
}

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

pub(crate) fn tooltip_bounds(anchor: Point, desired_size: Size, container: Rect, gap: f32) -> Rect {
    let fitted = Size::new(
        desired_size.width.min(container.size.width).max(0.0),
        desired_size.height.min(container.size.height).max(0.0),
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
    Rect::from_origin_size(
        Point::new(
            x.clamp(container.origin.x, maximum_x),
            y.clamp(container.origin.y, maximum_y),
        ),
        fitted,
    )
}

pub(crate) fn marker_bounds(center: Point, side: f32) -> Rect {
    let radius = side / 2.0;
    Rect::from_origin_size(
        Point::new(center.x - radius, center.y - radius),
        Size::new(radius * 2.0, radius * 2.0),
    )
}

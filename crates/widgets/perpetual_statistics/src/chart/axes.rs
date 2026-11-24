use smol_str::SmolStr;
use ui_core::math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px};

use super::{palette::AXIS_LINE_HEIGHT, rect::RectExt};

pub(crate) const MINIMUM_PLOT_WIDTH: LogicalPx = px(48.0);
pub(crate) const MINIMUM_AXIS_WIDTH: LogicalPx = px(42.0);
pub(crate) const AXIS_LABEL_HORIZONTAL_PADDING: LogicalPx = px(12.0);
pub(crate) const AXIS_TICK_TEXT_INSET: LogicalPx = px(8.0);
pub(crate) const CHART_MIN_CONTENT_WIDTH: LogicalPx = px(520.0);
pub(crate) const CHART_MIN_CONTENT_HEIGHT: LogicalPx = px(280.0);
pub(crate) const DATA_EDGE_INSET: LogicalPx = px(12.0);
pub(crate) const TIME_AXIS_HEIGHT: LogicalPx = px(28.0);
pub(crate) const LEGEND_HEIGHT: LogicalPx = px(30.0);
pub(crate) const TOP_INSET: LogicalPx = px(22.0);
pub(crate) const MAX_AXIS_TICKS: usize = 8;
pub(crate) const MAX_TIME_TICKS: usize = 6;
pub(crate) const TIME_TICK_WIDTH: LogicalPx = px(104.0);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartLayout {
    pub content: LogicalRect,
    pub left_axis: LogicalRect,
    pub plot: LogicalRect,
    pub right_axis: LogicalRect,
    pub time_axis: LogicalRect,
    pub legend: LogicalRect,
}

impl ChartLayout {
    pub(crate) fn new(
        bounds: LogicalRect,
        left_label_width: Option<LogicalPx>,
        right_label_width: LogicalPx,
    ) -> Self {
        let left_width = left_label_width.map_or(LogicalPx::ZERO, |width| {
            axis_width(bounds.size.width, width)
        });
        let right_width = axis_width(bounds.size.width, right_label_width);
        let content_width = bounds
            .size
            .width
            .max(MINIMUM_PLOT_WIDTH + left_width + right_width)
            .max(CHART_MIN_CONTENT_WIDTH);
        let content_height = bounds.size.height.max(CHART_MIN_CONTENT_HEIGHT);
        let content = LogicalRect::from_origin_size(
            bounds.origin,
            LogicalSize::new(content_width, content_height),
        );
        let time_height = TIME_AXIS_HEIGHT.min(content_height * 0.12);
        let legend_height = LEGEND_HEIGHT.min(content_height * 0.14);
        let top_inset = TOP_INSET
            .min((content_height - time_height - legend_height - px(1.0)).max(LogicalPx::ZERO));
        let plot_width = (content_width - left_width - right_width).max(px(1.0));
        let plot_height = (content_height - top_inset - time_height - legend_height).max(px(1.0));
        let plot = LogicalRect::from_origin_size(
            LogicalPoint::new(content.origin.x + left_width, content.origin.y + top_inset),
            LogicalSize::new(plot_width, plot_height),
        );
        Self {
            content,
            left_axis: LogicalRect::from_origin_size(
                LogicalPoint::new(content.origin.x, plot.origin.y),
                LogicalSize::new(left_width, plot_height),
            ),
            plot,
            right_axis: LogicalRect::from_origin_size(
                LogicalPoint::new(plot.right(), plot.origin.y),
                LogicalSize::new(right_width, plot_height),
            ),
            time_axis: LogicalRect::from_origin_size(
                LogicalPoint::new(plot.origin.x, plot.bottom()),
                LogicalSize::new(plot_width, time_height),
            ),
            legend: LogicalRect::from_origin_size(
                LogicalPoint::new(content.origin.x, plot.bottom() + time_height),
                LogicalSize::new(content.size.width, legend_height),
            ),
        }
    }
}

pub(crate) fn axis_width(chart_width: LogicalPx, longest_label: LogicalPx) -> LogicalPx {
    let desired =
        (px(longest_label.value().ceil()) + AXIS_LABEL_HORIZONTAL_PADDING).max(MINIMUM_AXIS_WIDTH);
    let available = (chart_width - MINIMUM_PLOT_WIDTH).max(LogicalPx::ZERO);
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
    pub origin_x: LogicalPx,
    pub width: LogicalPx,
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

pub(crate) fn value_y(value: f64, domain: ValueDomain, plot: LogicalRect) -> LogicalPx {
    let ratio = if domain.range() > 0.0 && value.is_finite() {
        ((value - domain.minimum) / domain.range()).clamp(0.0, 1.0)
    } else {
        0.5
    };
    plot.bottom() - plot.size.height * ratio as f32
}

pub(crate) fn data_x_range(plot: LogicalRect) -> (LogicalPx, LogicalPx) {
    let inset = DATA_EDGE_INSET.min(plot.size.width / 2.0);
    (plot.origin.x + inset, plot.right() - inset)
}

pub(crate) fn point_x(plot: LogicalRect, point_count: usize, index: usize) -> LogicalPx {
    if point_count <= 1 {
        return plot.origin.x + plot.size.width / 2.0;
    }
    let (left, right) = data_x_range(plot);
    let fraction = index.min(point_count - 1) as f32 / (point_count - 1) as f32;
    left + (right - left) * fraction
}

pub(crate) fn axis_label_y(y: LogicalPx, axis: LogicalRect) -> LogicalPx {
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

pub(crate) fn nearest_index(plot: LogicalRect, point_count: usize, x: LogicalPx) -> Option<usize> {
    if point_count == 0 || plot.size.width <= LogicalPx::ZERO {
        return None;
    }
    if point_count == 1 {
        return Some(0);
    }
    let (left, right) = data_x_range(plot);
    let width = right - left;
    if width <= LogicalPx::ZERO {
        return Some(0);
    }
    let fraction = ((x - left) / width).clamp(0.0, 1.0);
    Some((fraction * (point_count - 1) as f32).round() as usize)
}

pub(crate) fn paired_bar_bounds(
    plot: LogicalRect,
    point_count: usize,
    index: usize,
    buy: f64,
    sell: f64,
    domain: ValueDomain,
) -> (LogicalRect, LogicalRect) {
    let (buy_left, sell_left, bar_width) = paired_bar_x_bounds(plot, point_count, index);
    let baseline = value_y(0.0, domain, plot);
    let buy_top = value_y(buy, domain, plot);
    let sell_top = value_y(sell, domain, plot);
    (
        LogicalRect::from_origin_size(
            LogicalPoint::new(buy_left, buy_top),
            LogicalSize::new(bar_width, (baseline - buy_top).max(px(1.0))),
        ),
        LogicalRect::from_origin_size(
            LogicalPoint::new(sell_left, sell_top),
            LogicalSize::new(bar_width, (baseline - sell_top).max(px(1.0))),
        ),
    )
}

pub(crate) fn paired_bar_x_bounds(
    plot: LogicalRect,
    point_count: usize,
    index: usize,
) -> (LogicalPx, LogicalPx, LogicalPx) {
    let plot_width = plot.size.width;
    let slot_width = if point_count <= 1 {
        plot_width.min(px(32.0))
    } else {
        plot_width / point_count as f32
    };
    let group_width = (slot_width * 0.72).clamp(px(4.0), px(24.0));
    let bar_width = group_width / 2.0;
    let center = point_x(plot, point_count, index);
    (center - bar_width, center, bar_width)
}

pub(crate) fn ratio_bar_width(plot: LogicalRect, point_count: usize) -> LogicalPx {
    if point_count == 0 {
        return px(3.0);
    }
    (plot.size.width / point_count as f32 * 0.62).clamp(px(3.0), px(18.0))
}

pub(crate) fn stacked_bar_bounds(
    center_x: LogicalPx,
    width: LogicalPx,
    long_share: f64,
    short_share: f64,
    plot: LogicalRect,
) -> (LogicalRect, LogicalRect) {
    let total = long_share + short_share;
    let normalized_long = if total > 0.0 {
        (long_share / total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let plot_height = plot.size.height;
    let long_height = plot_height * normalized_long as f32;
    let short_height = plot_height - long_height;
    (
        LogicalRect::from_origin_size(
            LogicalPoint::new(center_x - width / 2.0, plot.bottom() - long_height),
            LogicalSize::new(width, long_height),
        ),
        LogicalRect::from_origin_size(
            LogicalPoint::new(center_x - width / 2.0, plot.origin.y),
            LogicalSize::new(width, short_height),
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

pub(crate) fn time_tick_indices(point_count: usize, plot_width: LogicalPx) -> Vec<usize> {
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
    let origin_limit = layout.time_axis.origin.x;
    labels
        .into_iter()
        .map(|(index, label)| {
            let x = point_x(layout.plot, point_count, index);
            PositionedTimeTick {
                label,
                origin_x: (x - width / 2.0).clamp(origin_limit, maximum_x),
                width,
            }
        })
        .collect()
}

pub(crate) fn tooltip_bounds(
    anchor: LogicalPoint,
    desired_size: LogicalSize,
    container: LogicalRect,
    gap: LogicalPx,
) -> LogicalRect {
    let fitted_width = desired_size
        .width
        .min(container.size.width)
        .max(LogicalPx::ZERO);
    let fitted_height = desired_size
        .height
        .min(container.size.height)
        .max(LogicalPx::ZERO);
    let mut x = anchor.x + gap;
    let mut y = anchor.y + gap;
    if x + fitted_width > container.right() {
        x = anchor.x - gap - fitted_width;
    }
    if y + fitted_height > container.bottom() {
        y = anchor.y - gap - fitted_height;
    }
    let maximum_x = (container.right() - fitted_width).max(container.origin.x);
    let maximum_y = (container.bottom() - fitted_height).max(container.origin.y);
    LogicalRect::from_origin_size(
        LogicalPoint::new(
            x.clamp(container.origin.x, maximum_x),
            y.clamp(container.origin.y, maximum_y),
        ),
        LogicalSize::new(fitted_width, fitted_height),
    )
}

pub(crate) fn marker_bounds(center: LogicalPoint, side: LogicalPx) -> LogicalRect {
    let radius = side / 2.0;
    LogicalRect::from_origin_size(
        LogicalPoint::new(center.x - radius, center.y - radius),
        LogicalSize::new(radius * 2.0, radius * 2.0),
    )
}

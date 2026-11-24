use smol_str::SmolStr;
use ui_core::math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px};

use super::{palette::AXIS_LINE_HEIGHT, units::RectExt};

pub(crate) const MINIMUM_PLOT_WIDTH: LogicalPx = px(48.0);
pub(crate) const MINIMUM_AXIS_WIDTH: LogicalPx = px(42.0);
pub(crate) const AXIS_LABEL_HORIZONTAL_PADDING: LogicalPx = px(12.0);
pub(crate) const AXIS_TICK_TEXT_INSET: LogicalPx = px(8.0);
pub(crate) const CHART_MIN_CONTENT_WIDTH: LogicalPx = px(640.0);
pub(crate) const CHART_MIN_CONTENT_HEIGHT: LogicalPx = px(360.0);
pub(crate) const DATA_EDGE_INSET: LogicalPx = px(12.0);
pub(crate) const TIME_AXIS_HEIGHT: LogicalPx = px(28.0);
pub(crate) const TOP_INSET: LogicalPx = px(22.0);
pub(crate) const MAX_AXIS_TICKS: usize = 8;
pub(crate) const MAX_TIME_TICKS: usize = 6;
pub(crate) const TIME_TICK_WIDTH: LogicalPx = px(108.0);

const MAXIMUM_BAR_WIDTH: LogicalPx = px(14.0);
const BAR_WIDTH_RATIO: f32 = 0.70;
const FLOW_HEADROOM_RATIO: f64 = 0.08;
const NET_ASSETS_HEADROOM_RATIO: f64 = 0.05;
const NET_ASSETS_ZERO_ANCHOR_MAX_SPREADS: f64 = 1.0;
pub(crate) const HOVER_MARKER_SIZE: LogicalPx = px(9.0);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChartLayout {
    pub content: LogicalRect,
    pub left_axis: LogicalRect,
    pub plot: LogicalRect,
    pub right_axis: LogicalRect,
    pub time_axis: LogicalRect,
}

impl ChartLayout {
    pub(crate) fn new(
        bounds: LogicalRect,
        left_label_width: LogicalPx,
        right_label_width: Option<LogicalPx>,
    ) -> Self {
        let left_width = axis_width(left_label_width);
        let right_width = right_label_width.map_or(LogicalPx::ZERO, axis_width);
        let content_width = bounds
            .size
            .width
            .max(content_min_size(left_label_width, right_label_width).width);
        let content_height = bounds.size.height.max(CHART_MIN_CONTENT_HEIGHT);
        let content = LogicalRect::new(
            bounds.origin.x,
            bounds.origin.y,
            content_width,
            content_height,
        );
        let time_height = TIME_AXIS_HEIGHT.min(content.size.height * 0.12);
        let top_inset = TOP_INSET.min((content.size.height - time_height - px(1.0)).max(px(0.0)));
        let plot_width = (content.size.width - left_width - right_width).max(px(1.0));
        let plot_height = (content.size.height - top_inset - time_height).max(px(1.0));
        let plot = LogicalRect::new(
            content.origin.x + left_width,
            content.origin.y + top_inset,
            plot_width,
            plot_height,
        );
        Self {
            content,
            left_axis: LogicalRect::new(content.origin.x, plot.origin.y, left_width, plot_height),
            plot,
            right_axis: LogicalRect::new(plot.right(), plot.origin.y, right_width, plot_height),
            time_axis: LogicalRect::new(plot.origin.x, plot.bottom(), plot_width, time_height),
        }
    }
}

pub(crate) fn axis_width(longest_label: LogicalPx) -> LogicalPx {
    (px(longest_label.value().ceil()) + AXIS_LABEL_HORIZONTAL_PADDING).max(MINIMUM_AXIS_WIDTH)
}

pub(crate) fn content_min_size(
    left_label_width: LogicalPx,
    right_label_width: Option<LogicalPx>,
) -> LogicalSize {
    let left = axis_width(left_label_width);
    let right = right_label_width.map_or(LogicalPx::ZERO, axis_width);
    LogicalSize::new(
        (left + right + MINIMUM_PLOT_WIDTH).max(CHART_MIN_CONTENT_WIDTH),
        CHART_MIN_CONTENT_HEIGHT,
    )
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

const DEGENERATE_DOMAIN: ValueDomain = ValueDomain::new(-1.0, 1.0);

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
    (nice_fraction * magnitude).max(f64::EPSILON)
}

pub(crate) fn value_y(value: f64, domain: ValueDomain, plot: LogicalRect) -> LogicalPx {
    let range = domain.range();
    if !range.is_finite() || range <= f64::EPSILON || !value.is_finite() {
        return plot.origin.y + plot.size.height / 2.0;
    }
    let ratio = ((value - domain.minimum) / range).clamp(0.0, 1.0) as f32;
    plot.bottom() - plot.size.height * ratio
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

pub(crate) fn nearest_index(plot: LogicalRect, point_count: usize, x: LogicalPx) -> Option<usize> {
    if point_count == 0 || plot.size.width <= px(0.0) {
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

pub(crate) fn axis_label_y(y: LogicalPx, axis: LogicalRect) -> LogicalPx {
    (y - AXIS_LINE_HEIGHT / 2.0).clamp(
        axis.origin.y,
        (axis.bottom() - AXIS_LINE_HEIGHT).max(axis.origin.y),
    )
}

pub(crate) fn flow_bar_bounds(
    plot: LogicalRect,
    point_count: usize,
    index: usize,
    value: f64,
    domain: ValueDomain,
) -> LogicalRect {
    let slot = plot.size.width / point_count as f32;
    let width = (slot * BAR_WIDTH_RATIO)
        .min(MAXIMUM_BAR_WIDTH)
        .max(px(1.0).min(slot));
    let center = point_x(plot, point_count, index);
    let baseline = value_y(0.0, domain, plot);
    let value_y = value_y(value, domain, plot);
    let top = baseline.min(value_y);
    let height = (baseline - value_y).abs().max(px(1.0));
    LogicalRect::new(center - width / 2.0, top, width, height)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PositionedTimeTick {
    pub label: SmolStr,
    pub origin_x: LogicalPx,
    pub width: LogicalPx,
}

pub(crate) fn position_time_ticks(
    labels: &[SmolStr],
    layout: ChartLayout,
) -> Box<[PositionedTimeTick]> {
    if labels.is_empty() {
        return Box::default();
    }
    let fitting = (layout.time_axis.size.width / TIME_TICK_WIDTH).floor();
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

fn time_ticks_are_disjoint(ticks: &[PositionedTimeTick]) -> bool {
    let mut previous_right: Option<LogicalPx> = None;
    for tick in ticks {
        if previous_right.is_some_and(|right| tick.origin_x < right) {
            return false;
        }
        previous_right = Some(tick.origin_x + tick.width);
    }
    true
}

pub(crate) fn tooltip_bounds(
    anchor: LogicalPoint,
    desired_size: LogicalSize,
    container: LogicalRect,
    gap: LogicalPx,
) -> LogicalRect {
    let fitted = LogicalSize::new(
        desired_size.width.min(container.size.width.max(px(0.0))),
        desired_size.height.min(container.size.height.max(px(0.0))),
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
    LogicalRect::new(
        x.clamp(container.origin.x, maximum_x),
        (anchor.y - fitted.height / 2.0).clamp(container.origin.y, maximum_y),
        fitted.width,
        fitted.height,
    )
}

pub(crate) fn marker_bounds(center: LogicalPoint) -> LogicalRect {
    let radius = HOVER_MARKER_SIZE / 2.0;
    LogicalRect::new(
        center.x - radius,
        center.y - radius,
        radius * 2.0,
        radius * 2.0,
    )
}

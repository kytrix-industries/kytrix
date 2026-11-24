use ui_core::math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize};

use super::geometry::{Sample, ScreenPoint, ValueAxis, y_for_series};

pub(crate) const LABEL_GAP: LogicalPx = LogicalPx::new(8.0);
pub(crate) const LABEL_RIGHT_MARGIN: LogicalPx = LogicalPx::new(4.0);
pub(crate) const LABEL_HEIGHT: LogicalPx = LogicalPx::new(20.0);
pub(crate) const LABEL_COLLISION_GAP: LogicalPx = LogicalPx::new(1.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LabelSide {
    Right,
    Left,
}

pub(crate) fn nearest_timestamp(timestamps: &[i64], time: i64) -> Option<i64> {
    match timestamps.binary_search(&time) {
        Ok(index) => Some(timestamps[index]),
        Err(index) => {
            let before = index.checked_sub(1).map(|before| timestamps[before]);
            let after = timestamps.get(index).copied();
            match (before, after) {
                (Some(before), Some(after)) => Some(if time - before <= after - time {
                    before
                } else {
                    after
                }),
                (Some(before), None) => Some(before),
                (None, Some(after)) => Some(after),
                (None, None) => None,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HoveredSample {
    pub timestamp: i64,
    pub x: LogicalPx,
}

pub(crate) fn value_at(samples: &[Sample], timestamp: i64) -> Option<f64> {
    let first = samples.first()?;
    if timestamp < first.timestamp {
        return None;
    }
    let index = samples.partition_point(|sample| sample.timestamp <= timestamp);
    index.checked_sub(1).map(|index| samples[index].percent)
}

pub(crate) fn hover_point(
    samples: &[Sample],
    axis: &ValueAxis,
    plot: LogicalRect,
    hovered: HoveredSample,
) -> Option<ScreenPoint> {
    let percent = value_at(samples, hovered.timestamp)?;
    Some(ScreenPoint {
        x: hovered.x,
        y: y_for_series(percent, axis, plot),
    })
}

pub(crate) fn label_side(
    crosshair_x: LogicalPx,
    widest_label: LogicalPx,
    plot: LogicalRect,
) -> LabelSide {
    let right = plot.origin.x + plot.size.width;
    if crosshair_x + LABEL_GAP + widest_label <= right - LABEL_RIGHT_MARGIN {
        LabelSide::Right
    } else {
        LabelSide::Left
    }
}

pub(crate) fn label_x(
    crosshair_x: LogicalPx,
    width: LogicalPx,
    side: LabelSide,
    plot: LogicalRect,
) -> LogicalPx {
    let natural = match side {
        LabelSide::Right => crosshair_x + LABEL_GAP,
        LabelSide::Left => crosshair_x - LABEL_GAP - width,
    };
    let left = plot.origin.x;
    let right = left + plot.size.width;
    natural.clamp(left, (right - width).max(left))
}

pub(crate) fn label_tops(
    anchors: &[LogicalPx],
    height: LogicalPx,
    gap: LogicalPx,
    plot: LogicalRect,
) -> Vec<LogicalPx> {
    let plot_top = plot.origin.y;
    let plot_bottom = plot_top + plot.size.height;
    let mut tops = vec![plot_top; anchors.len()];
    let mut order: Vec<usize> = (0..anchors.len()).collect();
    order.sort_by(|left, right| anchors[*right].value().total_cmp(&anchors[*left].value()));
    let mut top_below: Option<LogicalPx> = None;
    for index in order {
        let mut bottom = anchors[index].min(plot_bottom);
        if let Some(top_below) = top_below {
            bottom = bottom.min(top_below - gap);
        }
        let mut top = bottom - height;
        if top < plot_top {
            let inside_top = plot_top;
            let inside_bottom = inside_top + height;
            if top_below.is_none_or(|below| inside_bottom + gap <= below) {
                top = inside_top;
            }
        }
        tops[index] = top;
        top_below = Some(top);
    }
    tops
}

pub(crate) fn dot_bounds(center: ScreenPoint, diameter: LogicalPx) -> LogicalRect {
    let radius = diameter / 2.0;
    LogicalRect::from_origin_size(
        LogicalPoint::new(center.x - radius, center.y - radius),
        LogicalSize::new(diameter, diameter),
    )
}

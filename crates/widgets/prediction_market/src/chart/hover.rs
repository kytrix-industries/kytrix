use ui_core::math::{Point, Rect, Size};

use super::geometry::{Sample, ScreenPoint, ValueAxis, y_for_series};

pub(crate) const LABEL_GAP: f32 = 8.0;
pub(crate) const LABEL_RIGHT_MARGIN: f32 = 4.0;
pub(crate) const LABEL_HEIGHT: f32 = 20.0;
pub(crate) const LABEL_COLLISION_GAP: f32 = 1.0;

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
    pub x: f32,
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
    plot: Rect,
    hovered: HoveredSample,
) -> Option<ScreenPoint> {
    let percent = value_at(samples, hovered.timestamp)?;
    Some(ScreenPoint {
        x: hovered.x,
        y: y_for_series(percent, axis, plot),
    })
}

pub(crate) fn label_side(crosshair_x: f32, widest_label: f32, plot: Rect) -> LabelSide {
    if crosshair_x + LABEL_GAP + widest_label
        <= plot.origin.x + plot.size.width - LABEL_RIGHT_MARGIN
    {
        LabelSide::Right
    } else {
        LabelSide::Left
    }
}

pub(crate) fn label_x(crosshair_x: f32, width: f32, side: LabelSide, plot: Rect) -> f32 {
    let natural = match side {
        LabelSide::Right => crosshair_x + LABEL_GAP,
        LabelSide::Left => crosshair_x - LABEL_GAP - width,
    };
    let right = plot.origin.x + plot.size.width;
    natural.clamp(plot.origin.x, (right - width).max(plot.origin.x))
}

pub(crate) fn label_tops(anchors: &[f32], height: f32, gap: f32, plot: Rect) -> Vec<f32> {
    let mut tops = vec![plot.origin.y; anchors.len()];
    let mut order: Vec<usize> = (0..anchors.len()).collect();
    order.sort_by(|left, right| anchors[*right].total_cmp(&anchors[*left]));
    let mut top_below: Option<f32> = None;
    for index in order {
        let mut bottom = anchors[index].min(plot.origin.y + plot.size.height);
        if let Some(top_below) = top_below {
            bottom = bottom.min(top_below - gap);
        }
        let mut top = bottom - height;
        if top < plot.origin.y {
            let inside_top = plot.origin.y;
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

pub(crate) fn dot_bounds(center: ScreenPoint, diameter: f32) -> Rect {
    let radius = diameter / 2.0;
    Rect::from_origin_size(
        Point::new(center.x - radius, center.y - radius),
        Size::new(diameter, diameter),
    )
}

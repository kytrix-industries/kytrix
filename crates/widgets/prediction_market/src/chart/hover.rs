//! The pointer's arithmetic: which sample it is on, what that sample is worth, and
//! where the labels that name it go.
//!
//! Every function here is pure. A hover frame hands in a box and a position and gets
//! back boxes; nothing reads a window, so the placement rules are asserted directly
//! (`hover_tests.rs`) instead of through a rendered frame.
//!
//! The three rules are the reference chart's own:
//!
//! - the indicator **snaps to a sample**, it never stands between two of them, so the
//!   time under it is a time the market actually published;
//! - a sample's value is the step line's value, which is the value of the last sample
//!   at or before it — not an interpolation;
//! - a label sits **above its own point** and inside the plot when there is room; a
//!   column of labels is stacked from the bottom up so the lowest one keeps its point,
//!   and only a column too tall for the plot lets its upper boxes leave past 100%.

use gpui_kit::{Bounds, Pixels, point, px};

use super::geometry::{Sample, ScreenPoint, ValueAxis, y_for_series};

/// The gap between the indicator and the label column it opens.
pub(crate) const LABEL_GAP: f32 = 8.0;
/// The clear strip a right-hand label column keeps at the plot's edge.
pub(crate) const LABEL_RIGHT_MARGIN: f32 = 4.0;
/// The height of one hover label box.
pub(crate) const LABEL_HEIGHT: f32 = 20.0;
/// The gap a label keeps from the one below it in its column.
pub(crate) const LABEL_COLLISION_GAP: f32 = 1.0;

/// Which side of the indicator a label column is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LabelSide {
    Right,
    Left,
}

/// The timestamp nearest `time`.
///
/// The list is the sorted, unique set of every visible sample time across the
/// series, so all hover dots share one x. A tie goes to the earlier sample, which
/// keeps a pointer that sits exactly between two samples from flickering between
/// them as it moves a fraction of a pixel.
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

/// A one-point hover, already resolved against the prepared chart: the sample it
/// snapped to and the x the whole hover is drawn at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HoveredSample {
    pub timestamp: i64,
    pub x: Pixels,
}

/// The value a step line holds at `timestamp`.
///
/// It is the value of the last sample at or before `timestamp`; before a line's own
/// first sample there is no line yet, so there is no value to draw.
pub(crate) fn value_at(samples: &[Sample], timestamp: i64) -> Option<f64> {
    let first = samples.first()?;
    if timestamp < first.timestamp {
        return None;
    }
    let index = samples.partition_point(|sample| sample.timestamp <= timestamp);
    index.checked_sub(1).map(|index| samples[index].percent)
}

/// The screen point of one series at a hovered sample, if it has a value there.
pub(crate) fn hover_point(
    samples: &[Sample],
    axis: &ValueAxis,
    plot: Bounds<Pixels>,
    hovered: HoveredSample,
) -> Option<ScreenPoint> {
    let percent = value_at(samples, hovered.timestamp)?;
    Some(ScreenPoint {
        x: hovered.x,
        y: y_for_series(percent, axis, plot),
    })
}

/// Which side a label column opens on.
///
/// The right is preferred because it is what the reference chart does; the column
/// flips only when its widest label would not fit there with the four-pixel strip the
/// design keeps clear at the plot's right edge.
pub(crate) fn label_side(
    crosshair_x: Pixels,
    widest_label: Pixels,
    plot: Bounds<Pixels>,
) -> LabelSide {
    if crosshair_x + px(LABEL_GAP) + widest_label <= plot.right() - px(LABEL_RIGHT_MARGIN) {
        LabelSide::Right
    } else {
        LabelSide::Left
    }
}

/// The left edge of one label box in a column.
///
/// A right column starts at `crosshair_x + 8` for every box, and a left one ends at
/// `crosshair_x - 8` for every box, which is what keeps the column's edge straight
/// even though the boxes differ in width. A box too wide for either side is clamped
/// into the plot rather than drawn outside it.
pub(crate) fn label_x(
    crosshair_x: Pixels,
    width: Pixels,
    side: LabelSide,
    plot: Bounds<Pixels>,
) -> Pixels {
    let natural = match side {
        LabelSide::Right => crosshair_x + px(LABEL_GAP),
        LabelSide::Left => crosshair_x - px(LABEL_GAP) - width,
    };
    natural.clamp(plot.origin.x, (plot.right() - width).max(plot.origin.x))
}

/// The top of every label box of a column, in the order the boxes were handed in.
///
/// The natural bottom of a box is the y of the point it names, so a box sits
/// entirely above its point. The column is stacked from the bottom up: a box that
/// would overlap the one below it is pushed up until its bottom is one pixel above
/// that box's top. A box whose natural top would leave the plot is moved down to the
/// plot's top when the one-pixel gap to the box below still allows it; only a box
/// that cannot fit even there is left above the plot. The whole column is never
/// shifted because one box overflowed, which is what used to push a label at 0% out
/// below the x axis.
pub(crate) fn label_tops(
    anchors: &[Pixels],
    height: Pixels,
    gap: Pixels,
    plot: Bounds<Pixels>,
) -> Vec<Pixels> {
    let mut tops = vec![plot.origin.y; anchors.len()];
    let mut order: Vec<usize> = (0..anchors.len()).collect();
    // Stable, so two labels drawn at the same point keep the series order.
    order.sort_by(|left, right| f32::from(anchors[*right]).total_cmp(&f32::from(anchors[*left])));
    let mut top_below: Option<Pixels> = None;
    for index in order {
        let mut bottom = anchors[index].min(plot.bottom());
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

/// The hover dots' outer and inner boxes for one series at `center`.
///
/// Both are drawn as rounded quads: a filled circle is a quad whose corner radii are
/// half its side.
pub(crate) fn dot_bounds(center: ScreenPoint, diameter: Pixels) -> Bounds<Pixels> {
    let radius = diameter / 2.0;
    Bounds::new(
        point(center.x - radius, center.y - radius),
        gpui_kit::size(diameter, diameter),
    )
}

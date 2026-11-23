//! The pointer's arithmetic, asserted without a window: the sample a time snaps to,
//! the step value at a sample, and where the label column goes.

use ui_core::math::{Point, Rect, Size};

use super::{
    geometry::{RectExt, Sample, TimeDomain, ValueAxis, x_for_time, y_for_series},
    hover::{
        HoveredSample, LABEL_COLLISION_GAP, LABEL_GAP, LABEL_HEIGHT, LABEL_RIGHT_MARGIN, LabelSide,
        hover_point, label_side, label_tops, label_x, nearest_timestamp, value_at,
    },
};

fn plot() -> Rect {
    Rect::new(8.0, 18.0, 880.0, 200.0)
}

fn sample(timestamp: i64, percent: f64) -> Sample {
    Sample { timestamp, percent }
}

#[test]
fn the_nearest_sample_is_the_one_the_time_is_on() {
    let timestamps = [0, 60_000, 120_000, 180_000];

    assert_eq!(nearest_timestamp(&timestamps, 60_000), Some(60_000));
    assert_eq!(nearest_timestamp(&timestamps, 100_000), Some(120_000));
    assert_eq!(nearest_timestamp(&timestamps, 110_000), Some(120_000));
    assert_eq!(nearest_timestamp(&timestamps, -50_000), Some(0));
    assert_eq!(nearest_timestamp(&timestamps, 900_000), Some(180_000));
}

#[test]
fn a_tie_between_two_samples_goes_to_the_earlier() {
    let timestamps = [0, 60_000];

    assert_eq!(nearest_timestamp(&timestamps, 30_000), Some(0));
}

#[test]
fn an_empty_sample_set_has_no_nearest_sample() {
    assert_eq!(nearest_timestamp(&[], 0), None);
}

#[test]
fn a_step_value_before_the_first_sample_does_not_exist() {
    let samples = [sample(1_000, 10.0), sample(2_000, 20.0)];

    assert_eq!(value_at(&samples, 999), None);
    assert_eq!(value_at(&samples, 1_000), Some(10.0));
}

#[test]
fn a_step_value_between_samples_is_the_one_before_it() {
    let samples = [
        sample(1_000, 10.0),
        sample(2_000, 20.0),
        sample(3_000, 30.0),
    ];

    assert_eq!(value_at(&samples, 1_500), Some(10.0));
    assert_eq!(value_at(&samples, 2_500), Some(20.0));
    assert_eq!(value_at(&samples, 3_000), Some(30.0));
    assert_eq!(
        value_at(&samples, 9_000),
        Some(30.0),
        "a step line holds its last value after the last sample"
    );
}

#[test]
fn a_duplicated_timestamp_reports_the_last_sample_at_it() {
    let samples = [
        sample(1_000, 10.0),
        sample(1_000, 15.0),
        sample(2_000, 20.0),
    ];

    assert_eq!(value_at(&samples, 1_000), Some(15.0));
}

#[test]
fn the_hover_point_is_at_the_snapped_x_and_the_step_value() {
    let samples = [sample(0, 10.0), sample(1_000, 30.0)];
    let axis = ValueAxis::new(30.0);
    let hovered = HoveredSample {
        timestamp: 500,
        x: 400.0,
    };

    let point =
        hover_point(&samples, &axis, plot(), hovered).expect("the series has a value at the time");

    assert_eq!(point.x, 400.0);
    assert_eq!(point.y, y_for_series(10.0, &axis, plot()));
}

#[test]
fn the_label_column_prefers_the_right() {
    assert_eq!(label_side(100.0, 80.0, plot()), LabelSide::Right);
    assert_eq!(
        label_x(100.0, 80.0, LabelSide::Right, plot()),
        100.0 + LABEL_GAP
    );
}

#[test]
fn the_label_column_flips_left_when_the_widest_label_does_not_fit() {
    let plot = plot();
    let crosshair = plot.right() - 20.0;
    let widest = 120.0;

    assert_eq!(
        label_side(crosshair, widest, plot),
        LabelSide::Left,
        "a label that runs past the plot's edge flips its whole column"
    );
    assert_eq!(
        label_x(crosshair, widest, LabelSide::Left, plot),
        crosshair - LABEL_GAP - widest
    );
}

#[test]
fn a_label_column_flips_at_the_plots_four_pixel_edge_strip() {
    let plot = plot();
    let widest = 100.0;
    let fits = plot.right() - LABEL_RIGHT_MARGIN - LABEL_GAP - widest;

    assert_eq!(
        label_side(fits, widest, plot),
        LabelSide::Right,
        "a column ending exactly on the plot's four-pixel strip still fits"
    );
    assert_eq!(
        label_side(fits + 1.0, widest, plot),
        LabelSide::Left,
        "one pixel further and the whole column flips left"
    );
}

#[test]
fn a_label_too_wide_for_either_side_is_clamped_into_the_plot() {
    let plot = plot();
    let crosshair = plot.origin.x + 4.0;
    let width = 870.0;

    assert_eq!(
        label_x(crosshair, width, LabelSide::Left, plot),
        plot.origin.x
    );
    assert_eq!(
        label_x(plot.right() - 4.0, width, LabelSide::Right, plot),
        plot.right() - width,
        "a right column wider than the plot is clamped to its right edge"
    );
}

#[test]
fn every_label_box_sits_above_its_own_point() {
    let tops = label_tops(&[200.0, 100.0], LABEL_HEIGHT, LABEL_COLLISION_GAP, plot());

    assert_eq!(tops[0] + LABEL_HEIGHT, 200.0);
    assert_eq!(tops[1] + LABEL_HEIGHT, 100.0);
}

#[test]
fn a_collision_pushes_the_upper_box_up_with_a_one_pixel_gap() {
    let plot = plot();
    let tops = label_tops(&[100.0, 120.0], LABEL_HEIGHT, LABEL_COLLISION_GAP, plot);

    // The box of the lower point keeps its own bottom; the upper one gives way.
    assert_eq!(tops[1] + LABEL_HEIGHT, 120.0, "the lower box is not moved");
    assert_eq!(
        tops[0] + LABEL_HEIGHT,
        tops[1] - LABEL_COLLISION_GAP,
        "the upper box ends one pixel above the lower one"
    );
}

#[test]
fn a_label_at_either_end_stays_inside_until_the_column_is_crowded() {
    let plot = plot();
    let tops = label_tops(
        &[plot.bottom(), plot.origin.y],
        LABEL_HEIGHT,
        LABEL_COLLISION_GAP,
        plot,
    );

    assert_eq!(
        tops[0] + LABEL_HEIGHT,
        plot.bottom(),
        "the 0% label keeps its own bottom instead of being pushed below the axis"
    );
    assert_eq!(
        tops[1], plot.origin.y,
        "the 100% label is moved down to the plot's top"
    );
    assert!(
        tops.iter()
            .all(|top| *top >= plot.origin.y && *top + LABEL_HEIGHT <= plot.bottom()),
        "both labels stay inside a plot with room for them: {tops:?}"
    );
}

#[test]
fn a_saturated_column_only_lets_its_overflow_leave_the_plot() {
    let plot = plot();

    let anchors: Vec<f32> = (0..8).map(|index| 30.0 + index as f32 * 2.0).collect();
    let tops = label_tops(&anchors, LABEL_HEIGHT, LABEL_COLLISION_GAP, plot);
    let mut sorted = tops.clone();
    sorted.sort_by(|left, right| left.total_cmp(right));

    assert!(
        sorted.windows(2).all(|pair| pair[1] > pair[0]),
        "the boxes are not clamped into one pile: {tops:?}"
    );
    assert!(
        sorted[sorted.len() - 1] + LABEL_HEIGHT <= plot.bottom(),
        "the lowest box stays inside the plot: {tops:?}"
    );
    assert!(
        sorted[0] < plot.origin.y,
        "only the boxes that cannot fit above it leave the plot: {tops:?}"
    );
    assert!(
        sorted
            .windows(2)
            .all(|pair| ((pair[1] - pair[0]) - 21.0).abs() < 0.01),
        "the one-pixel gaps survive the overflow: {tops:?}"
    );
}

#[test]
fn a_twelve_series_column_keeps_every_label_at_a_distinct_top() {
    let plot = plot();
    let anchors: Vec<f32> = (0..12)
        .map(|index| plot.bottom() - index as f32 * 4.0)
        .collect();
    let tops = label_tops(&anchors, LABEL_HEIGHT, LABEL_COLLISION_GAP, plot);
    let mut sorted = tops.clone();
    sorted.sort_by(|left, right| left.total_cmp(right));

    assert_eq!(sorted.len(), 12);
    assert!(
        sorted.windows(2).all(|pair| pair[1] > pair[0]),
        "twelve boxes keep twelve distinct tops: {tops:?}"
    );
    assert!(
        sorted[sorted.len() - 1] + LABEL_HEIGHT <= plot.bottom(),
        "the lowest box stays inside the plot"
    );
    assert!(
        sorted
            .windows(2)
            .all(|pair| ((pair[1] - pair[0]) - 21.0).abs() < 0.01),
        "the one-pixel gaps survive the overflow: {tops:?}"
    );
}

#[test]
fn the_box_anchor_is_the_series_point_y() {
    let axis = ValueAxis::new(31.5);
    let anchor = y_for_series(26.5, &axis, plot());
    let tops = label_tops(&[anchor], LABEL_HEIGHT, 1.0, plot());

    assert_eq!(tops[0] + LABEL_HEIGHT, anchor);
}

#[test]
fn a_snapped_x_is_the_sample_time_on_the_scale() {
    let domain = TimeDomain {
        start: 0,
        end: 1_000,
    };
    let plot = plot();
    let x = x_for_time(domain, plot, 500);

    assert_eq!(x, plot.origin.x + plot.size.width / 2.0);
}

/// The dot a hover draws is a rounded quad centred on the series point.
#[test]
fn a_dot_is_centred_on_its_screen_point() {
    let dot = super::hover::dot_bounds(super::geometry::ScreenPoint { x: 100.0, y: 50.0 }, 12.0);

    assert_eq!(dot, Rect::new(94.0, 44.0, 12.0, 12.0));
    assert_eq!(
        (
            dot.origin.x + dot.size.width / 2.0,
            dot.origin.y + dot.size.height / 2.0
        ),
        (100.0, 50.0)
    );
}

#[test]
fn the_plot_edges_are_the_origin_plus_the_size() {
    let plot = Rect::from_origin_size(Point::new(3.0, 7.0), Size::new(40.0, 20.0));

    assert_eq!(plot.right(), 43.0);
    assert_eq!(plot.bottom(), 27.0);
}

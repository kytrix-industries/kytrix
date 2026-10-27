//! The two domains, the tick values and the boxes one ETF chart is built from.
//!
//! Everything here is a pure function of its arguments, which is what makes the
//! geometry testable at all: a layout is derived from a box and a few measured
//! widths, and the same inputs always produce the same boxes.

use gpui_kit::{Bounds, Pixels, point, px, size};
use smol_str::{SmolStr, format_smolstr};

use super::axes::*;

fn plot() -> Bounds<Pixels> {
    Bounds::new(point(px(10.0), px(20.0)), size(px(300.0), px(200.0)))
}

/// Dated labels, as the time axis holds them.
fn labels(count: usize) -> Vec<SmolStr> {
    (1..=count)
        .map(|day| format_smolstr!("01-{day:02}"))
        .collect()
}

#[test]
fn a_flow_domain_should_include_zero_and_headroom() {
    let domain = flow_domain([-10.0, 20.0].into_iter());

    assert!(domain.minimum < -10.0);
    assert!(domain.maximum > 20.0);
    assert!(domain.minimum < 0.0 && domain.maximum > 0.0);
}

#[test]
fn an_empty_flow_domain_should_be_symmetric() {
    assert_eq!(flow_domain([].into_iter()), ValueDomain::new(-1.0, 1.0));
}

#[test]
fn a_flow_domain_should_stay_finite_for_extreme_readings() {
    let domain = flow_domain([f64::MAX, -f64::MAX].into_iter());
    let assets = net_assets_domain([f64::MAX, -f64::MAX].into_iter());

    assert!(domain.minimum.is_finite() && domain.maximum.is_finite());
    assert!(assets.minimum.is_finite() && assets.maximum.is_finite());
}

#[test]
fn the_axis_should_anchor_a_low_positive_history_at_zero() {
    let domain = net_assets_domain([655.3, 62_735.9].into_iter());
    let values = AxisScale::new(domain, |value, _| SmolStr::new(format!("{value}")));

    assert_eq!(domain.minimum, 0.0);
    assert!(domain.maximum > 62_735.9);
    assert_eq!(
        values
            .ticks
            .iter()
            .map(|tick| tick.value)
            .collect::<Vec<_>>(),
        vec![
            0.0, 10_000.0, 20_000.0, 30_000.0, 40_000.0, 50_000.0, 60_000.0
        ]
    );
    assert!(
        values
            .ticks
            .iter()
            .all(|tick| domain.minimum <= tick.value && tick.value <= domain.maximum)
    );
}

#[test]
fn the_axis_should_anchor_a_near_zero_history_at_the_plot_bottom() {
    let domain = net_assets_domain([3.0, 15.8].into_iter());
    let values = AxisScale::new(domain, |value, _| SmolStr::new(format!("{value}")));

    assert_eq!(domain.minimum, 0.0);
    assert!(domain.maximum > 15.8);
    assert_eq!(
        values
            .ticks
            .iter()
            .map(|tick| tick.value)
            .collect::<Vec<_>>(),
        vec![0.0, 5.0, 10.0, 15.0]
    );
    assert_eq!(value_y(0.0, domain, plot()), plot().bottom());
}

#[test]
fn the_axis_should_keep_zero_inside_a_crossing_domain() {
    let domain = net_assets_domain([-686.9, 15_110.3].into_iter());
    let values = AxisScale::new(domain, |value, _| SmolStr::new(format!("{value}")));

    assert!((domain.minimum + 721.245).abs() < 1e-9);
    assert!((domain.maximum - 15_865.815).abs() < 1e-9);
    assert!(values.ticks.iter().any(|tick| tick.value == 0.0));
    assert_eq!(
        values
            .ticks
            .iter()
            .map(|tick| tick.value)
            .collect::<Vec<_>>(),
        vec![0.0, 5_000.0, 10_000.0, 15_000.0]
    );
    let zero_y = value_y(0.0, domain, plot());
    assert!(zero_y > plot().origin.y && zero_y < plot().bottom());
    let zero_fraction = f64::from(plot().bottom() - zero_y) / f64::from(plot().size.height);
    let expected_fraction = -domain.minimum / domain.range();
    assert!((zero_fraction - expected_fraction).abs() < 1e-6);
}

#[test]
fn a_crossing_domain_should_pad_its_two_sides_independently() {
    let domain = net_assets_domain([-1.0, 100.0].into_iter());

    assert!((domain.minimum + 1.05).abs() < 1e-9);
    assert!((domain.maximum - 105.0).abs() < 1e-9);
}

#[test]
fn the_axis_should_anchor_a_near_zero_negative_history_at_zero() {
    let domain = net_assets_domain([-100.0, -0.1].into_iter());

    assert_eq!(domain.maximum, 0.0);
    assert!(domain.minimum < -100.0);
    assert_eq!(value_y(0.0, domain, plot()), plot().origin.y);
    assert!(
        AxisScale::new(domain, |value, _| SmolStr::new(format!("{value}")))
            .ticks
            .iter()
            .any(|tick| tick.value == 0.0)
    );
}

#[test]
fn a_far_negative_baseline_window_should_keep_its_own_axis() {
    let domain = net_assets_domain([-100.0, -90.0].into_iter());
    let values = AxisScale::new(domain, |value, _| SmolStr::new(format!("{value}")));

    assert!(domain.maximum < 0.0);
    assert!(!values.ticks.iter().any(|tick| tick.value == 0.0));
}

#[test]
fn a_high_baseline_window_should_not_be_flattened_against_zero() {
    let domain = net_assets_domain([50_904.6, 54_317.8].into_iter());
    let values = AxisScale::new(domain, |value, _| SmolStr::new(format!("{value}")));

    assert!(domain.minimum > 50_000.0);
    assert!(domain.maximum < 55_000.0);
    assert!(domain.range() < 5_000.0);
    assert!(!values.ticks.iter().any(|tick| tick.value == 0.0));
}

#[test]
fn a_constant_history_should_get_headroom_of_its_own() {
    assert_eq!(
        net_assets_domain([50_000.0, 50_000.0].into_iter()),
        ValueDomain::new(47_500.0, 52_500.0)
    );
}

#[test]
fn a_reading_that_is_not_finite_should_not_stretch_an_axis() {
    let domain = net_assets_domain([f64::NAN, f64::INFINITY, 5.0].into_iter());
    let flow = flow_domain([f64::NAN, 7.0].into_iter());

    assert_eq!(
        domain,
        ValueDomain::new(4.75, 5.25),
        "one finite reading is a window that did not move"
    );
    assert!(flow.minimum <= 0.0 && flow.maximum >= 7.0);
    assert!(flow.maximum.is_finite());
}

#[test]
fn values_that_are_not_readings_should_not_reach_an_axis_at_all() {
    let domain = net_assets_domain([f64::NAN, f64::INFINITY].into_iter());

    assert_eq!(domain, ValueDomain::new(0.0, 1.0));
}

#[test]
fn axis_ticks_should_be_finite_strictly_ordered_and_normalize_negative_zero() {
    let scale = AxisScale::new(ValueDomain::new(-1_476.0, 15_900.0), |value, _| {
        SmolStr::new(format!("{value}"))
    });

    assert!(scale.ticks.iter().all(|tick| tick.value.is_finite()));
    assert!(
        scale
            .ticks
            .windows(2)
            .all(|ticks| ticks[0].value < ticks[1].value)
    );
    let zero = scale
        .ticks
        .iter()
        .find(|tick| tick.value == 0.0)
        .expect("the zero baseline should have a tick");
    assert!(zero.value.is_sign_positive());
}

#[test]
fn a_decimal_axis_should_normalize_an_accumulated_negative_zero() {
    let scale = AxisScale::new(flow_domain([-1.0].into_iter()), |value, step| {
        super::text::format_axis_value(value, step)
    });
    let zero = scale
        .ticks
        .iter()
        .find(|tick| tick.value == 0.0)
        .expect("a zero flow baseline should be retained");

    assert!(zero.value.is_sign_positive());
    assert_eq!(zero.label, "0.0");
}

#[test]
fn an_axis_should_not_drop_an_in_domain_zero_baseline() {
    let scale = AxisScale::new(ValueDomain::new(-700.0, 0.0), |value, _| {
        SmolStr::new(format!("{value}"))
    });

    assert!(scale.ticks.iter().any(|tick| tick.value == 0.0));
    assert!(scale.ticks.len() <= MAX_AXIS_TICKS);
}

#[test]
fn nearest_index_should_clamp_to_the_plot_edges() {
    assert_eq!(nearest_index(plot(), 5, px(-50.0)), Some(0));
    assert_eq!(nearest_index(plot(), 5, px(500.0)), Some(4));
    assert_eq!(nearest_index(plot(), 0, px(20.0)), None);
}

#[test]
fn the_first_and_last_points_should_keep_the_edge_inset() {
    let plot = plot();

    assert_eq!(point_x(plot, 5, 0), plot.origin.x + DATA_EDGE_INSET);
    assert_eq!(point_x(plot, 5, 4), plot.right() - DATA_EDGE_INSET);
}

#[test]
fn a_single_point_should_be_centered_in_the_plot() {
    assert_eq!(
        point_x(plot(), 1, 0),
        plot().origin.x + plot().size.width / 2.0
    );
}

#[test]
fn value_y_should_map_zero_between_inflows_and_outflows() {
    let domain = ValueDomain::new(-10.0, 10.0);

    assert_eq!(value_y(0.0, domain, plot()), px(120.0));
}

#[test]
fn axis_labels_should_be_centered_and_clamped_inside_the_axis() {
    let axis = plot();

    assert_eq!(
        axis_label_y(px(120.0), axis),
        px(120.0) - super::palette::AXIS_LINE_HEIGHT / 2.0
    );
    assert_eq!(axis_label_y(axis.origin.y, axis), axis.origin.y);
    assert_eq!(
        axis_label_y(axis.bottom(), axis),
        axis.bottom() - super::palette::AXIS_LINE_HEIGHT
    );
}

#[test]
fn a_bar_should_grow_from_the_zero_line_whichever_way_it_points() {
    let plot = plot();
    let domain = ValueDomain::new(-10.0, 10.0);

    let inflow = flow_bar_bounds(plot, 5, 2, 10.0, domain);
    let outflow = flow_bar_bounds(plot, 5, 2, -10.0, domain);
    let baseline = value_y(0.0, domain, plot);

    assert_eq!(
        inflow.bottom(),
        baseline,
        "an inflow stands on the baseline"
    );
    assert_eq!(inflow.top(), plot.origin.y);
    assert_eq!(outflow.origin.y, baseline, "an outflow hangs from it");
    assert_eq!(outflow.bottom(), plot.bottom());
    assert_eq!(inflow.origin.x, outflow.origin.x, "one bar per day");
    assert!(inflow.size.width >= px(1.0));
}

#[test]
fn a_layout_should_measure_both_axes_and_drop_an_absent_one() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(640.0), px(360.0)));
    let dual = ChartLayout::new(bounds, px(80.0), Some(px(100.0)));
    let single = ChartLayout::new(bounds, px(80.0), None);

    assert_eq!(dual.left_axis.size.width, px(92.0));
    assert_eq!(dual.right_axis.size.width, px(112.0));
    assert_eq!(single.left_axis.size.width, px(92.0));
    assert_eq!(single.right_axis.size.width, Pixels::ZERO);
    assert_eq!(dual.plot.origin.y - bounds.origin.y, TOP_INSET);
    assert_eq!(dual.time_axis.origin.y, dual.plot.bottom());
    assert_eq!(dual.content.origin, bounds.origin);
}

#[test]
fn a_layout_should_grow_its_content_rather_than_squeeze_a_label() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(360.0)));
    let layout = ChartLayout::new(bounds, px(500.0), Some(px(500.0)));

    assert_eq!(layout.left_axis.size.width, px(512.0));
    assert_eq!(layout.right_axis.size.width, px(512.0));
    assert_eq!(layout.plot.size.width, MINIMUM_PLOT_WIDTH);
    assert_eq!(layout.content.size.width, px(1_072.0));
}

#[test]
fn a_layout_should_never_be_smaller_than_the_chart_it_draws() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(120.0), px(90.0)));
    let layout = ChartLayout::new(bounds, px(20.0), Some(px(20.0)));

    assert_eq!(layout.content.size.width, CHART_MIN_CONTENT_WIDTH);
    assert_eq!(layout.content.size.height, CHART_MIN_CONTENT_HEIGHT);
    assert!(layout.plot.size.width > Pixels::ZERO);
    assert!(layout.plot.size.height > Pixels::ZERO);
}

/// The minimum a chart asks its pane to scroll to is what its own labels need.
///
/// It is not a function of the pane, which is the point of it: a minimum that grew
/// with the box the last frame was laid out in could never shrink again, and a pane
/// that was restored from a maximised window would keep drawing at the width it had
/// when it was enlarged.
#[test]
fn the_content_minimum_should_account_for_both_axes() {
    assert_eq!(
        content_min_size(px(80.0), Some(px(100.0))),
        size(px(640.0), px(360.0)),
        "labels that fit the chart's floor report the floor"
    );
    assert_eq!(
        content_min_size(px(500.0), Some(px(500.0))),
        size(px(1_072.0), px(360.0)),
        "labels wider than the floor grow the content to them"
    );
    assert_eq!(
        content_min_size(px(700.0), None),
        size(px(760.0), px(360.0)),
        "a chart with one axis needs that axis' width and the plot beside it"
    );
}

#[test]
fn the_time_axis_should_label_both_ends_and_never_overlap() {
    let labels = labels(30);
    let wide = ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(640.0), px(360.0))),
        px(80.0),
        Some(px(100.0)),
    );
    let narrow = ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(360.0))),
        px(500.0),
        Some(px(500.0)),
    );

    let wide_ticks = position_time_ticks(&labels, wide);
    assert!(wide_ticks.len() > 1, "a wide plot labels more than one day");
    assert_eq!(
        wide_ticks.first().map(|tick| tick.label.as_str()),
        Some("01-01")
    );
    assert_eq!(
        wide_ticks.last().map(|tick| tick.label.as_str()),
        Some("01-30")
    );
    assert!(
        wide_ticks
            .windows(2)
            .all(|ticks| ticks[0].origin_x + ticks[0].width <= ticks[1].origin_x),
        "one label may not sit on another"
    );

    let narrow_ticks = position_time_ticks(&labels, narrow);
    assert_eq!(
        narrow_ticks.len(),
        1,
        "a plot with room for one label shows one rather than overlapping several"
    );
    assert_eq!(
        narrow_ticks.first().map(|tick| tick.label.as_str()),
        Some("01-01")
    );
}

#[test]
fn an_empty_window_should_label_no_days() {
    let layout = ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(640.0), px(360.0))),
        px(80.0),
        Some(px(100.0)),
    );

    assert!(position_time_ticks(&[], layout).is_empty());
}

#[test]
fn a_tooltip_should_fit_inside_a_narrow_plot() {
    let narrow = Bounds::new(point(px(0.0), px(0.0)), size(px(24.0), px(40.0)));
    let bounds = tooltip_bounds(
        point(px(12.0), px(20.0)),
        size(px(230.0), px(82.0)),
        narrow,
        px(10.0),
    );

    assert!(bounds.size.width <= narrow.size.width);
    assert!(bounds.size.height <= narrow.size.height);
    assert!(bounds.origin.x >= narrow.origin.x);
    assert!(bounds.origin.y >= narrow.origin.y);
}

#[test]
fn a_tooltip_should_flip_side_rather_than_leave_the_plot() {
    let plot = plot();
    let at_right = tooltip_bounds(
        point(plot.right() - px(2.0), plot.origin.y),
        size(px(60.0), px(40.0)),
        plot,
        px(10.0),
    );
    let at_left = tooltip_bounds(
        point(plot.origin.x + px(2.0), plot.origin.y),
        size(px(60.0), px(40.0)),
        plot,
        px(10.0),
    );

    assert!(at_right.right() <= plot.right());
    assert!(
        at_right.origin.x < plot.right() - px(2.0),
        "it moves left of the pointer"
    );
    assert!(at_left.origin.x >= plot.origin.x);
    assert!(
        at_left.origin.x > plot.origin.x + px(2.0),
        "it moves right of the pointer"
    );
}

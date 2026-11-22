//! Where everything on a statistics chart goes.
//!
//! The layout is a pure function of the box the chart was given and the labels its axes
//! will show, so these tests need no engine: they are the whole of what decides where a
//! box lands.

use smol_str::SmolStr;
use ui_core::math::{Point, Rect, Size};

use super::{
    axes::{
        CHART_MIN_CONTENT_HEIGHT, CHART_MIN_CONTENT_WIDTH, ChartLayout, DATA_EDGE_INSET,
        MAX_AXIS_TICKS, MINIMUM_AXIS_WIDTH, MINIMUM_PLOT_WIDTH, TIME_AXIS_HEIGHT, ValueDomain,
        axis_label_y, axis_width, bar_domain, label_indices, line_domain, nearest_index, nice_step,
        paired_bar_bounds, paired_bar_x_bounds, point_x, position_time_ticks, ratio_bar_width,
        ratio_domain, stacked_bar_bounds, time_tick_indices, tooltip_bounds, value_y,
    },
    palette::AXIS_LINE_HEIGHT,
    rect::RectExt,
};

/// The box a chart is laid out in for most of these tests.
fn bounds() -> Rect {
    Rect::new(0.0, 0.0, 640.0, 360.0)
}

/// A box's centre.
fn center(plot: Rect) -> Point {
    Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )
}

#[test]
fn the_time_axis_should_sit_under_the_plot_and_the_legend_under_that() {
    let layout = ChartLayout::new(bounds(), Some(60.0), 60.0);

    assert_eq!(layout.time_axis.origin.y, layout.plot.bottom());
    assert_eq!(layout.time_axis.size.height, TIME_AXIS_HEIGHT);
    assert_eq!(layout.legend.origin.y, layout.time_axis.bottom());
    assert_eq!(layout.legend.origin.x, layout.content.origin.x);
    assert_eq!(layout.legend.size.width, layout.content.size.width);
}

#[test]
fn each_axis_should_share_the_plot_height_and_sit_at_its_own_edge() {
    let layout = ChartLayout::new(bounds(), Some(60.0), 50.0);

    assert_eq!(layout.left_axis.origin.y, layout.plot.origin.y);
    assert_eq!(layout.left_axis.size.height, layout.plot.size.height);
    assert_eq!(layout.left_axis.right(), layout.plot.origin.x);
    assert_eq!(layout.right_axis.origin.x, layout.plot.right());
    assert_eq!(layout.right_axis.size.height, layout.plot.size.height);
}

#[test]
fn a_short_label_should_leave_its_axis_at_the_minimum_width() {
    assert_eq!(axis_width(640.0, 12.0), MINIMUM_AXIS_WIDTH);
}

#[test]
fn a_long_label_should_widen_only_its_own_axis() {
    let narrow = ChartLayout::new(bounds(), None, 20.0);
    let wide = ChartLayout::new(bounds(), None, 80.0);

    assert_eq!(narrow.left_axis.size.width, 0.0);
    assert_eq!(
        wide.right_axis.size.width,
        (80.0_f32 + 12.0).min(640.0 - MINIMUM_PLOT_WIDTH)
    );
    assert_eq!(
        narrow.plot.size.width - wide.plot.size.width,
        wide.right_axis.size.width - narrow.right_axis.size.width
    );
}

#[test]
fn a_chart_without_a_left_axis_should_give_that_width_to_the_plot() {
    let layout = ChartLayout::new(bounds(), None, 60.0);

    assert_eq!(layout.left_axis.size.width, 0.0);
    assert_eq!(
        layout.plot.size.width,
        layout.content.size.width - layout.right_axis.size.width
    );
}

#[test]
fn a_narrow_pane_should_grow_the_content_instead_of_squeezing_the_plot() {
    let layout = ChartLayout::new(Rect::new(0.0, 0.0, 200.0, 120.0), Some(60.0), 60.0);

    assert_eq!(layout.content.size.width, CHART_MIN_CONTENT_WIDTH);
    assert_eq!(layout.content.size.height, CHART_MIN_CONTENT_HEIGHT);
    assert!(
        layout.plot.size.width >= MINIMUM_PLOT_WIDTH,
        "the plot keeps its minimum"
    );
}

#[test]
fn a_pane_too_narrow_for_its_axes_should_still_keep_a_usable_plot() {
    let layout = ChartLayout::new(Rect::new(0.0, 0.0, 20.0, 20.0), Some(200.0), 200.0);

    assert!(
        layout.plot.size.width >= 1.0,
        "a chart of no width still resolves a plot"
    );
    assert!(layout.plot.size.height >= 1.0);
}

#[test]
fn a_short_pane_should_keep_the_time_axis_and_the_legend_inside_the_content() {
    let layout = ChartLayout::new(Rect::new(0.0, 0.0, 640.0, 60.0), None, 60.0);

    assert!(layout.plot.size.height >= 1.0);
    assert!(
        layout.legend.bottom() <= layout.content.bottom(),
        "the legend stays inside the content box"
    );
}

#[test]
fn a_line_domain_should_keep_room_around_its_data() {
    let domain = line_domain([10.0, 20.0, 30.0].into_iter());

    assert!(domain.minimum < 10.0);
    assert!(domain.maximum > 30.0);
    assert!(domain.minimum >= 0.0, "a line domain never goes below zero");
}

#[test]
fn a_line_domain_should_ignore_what_is_not_a_reading() {
    let domain = line_domain([10.0, f64::NAN, -5.0, f64::INFINITY].into_iter());

    assert!(domain.minimum < 10.0);
    assert!(domain.maximum > 10.0);
}

#[test]
fn a_bar_domain_should_start_at_zero_and_leave_headroom() {
    let domain = bar_domain([0.5, 2.0].into_iter());

    assert_eq!(domain.minimum, 0.0);
    assert!(domain.maximum > 2.0);
}

#[test]
fn a_ratio_domain_should_stay_finite_for_a_constant_series() {
    let domain = ratio_domain([2.0, 2.0, 2.0].into_iter());

    assert!(domain.minimum < 2.0 && domain.maximum > 2.0);
}

#[test]
fn a_ratio_domain_should_reject_values_that_are_not_ratios() {
    let domain = ratio_domain([0.0, -1.0, f64::NAN].into_iter());

    assert_eq!(domain, ValueDomain::new(0.0, 1.0));
}

#[test]
fn axis_ticks_should_be_ordered_finite_and_inside_the_domain() {
    let domain = ValueDomain::new(0.0, 100.0);
    let ticks = super::axes::nice_ticks(domain);

    assert!(ticks.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(ticks.len() <= MAX_AXIS_TICKS);
    assert!(
        ticks
            .iter()
            .all(|tick| *tick >= domain.minimum && *tick <= domain.maximum)
    );
}

#[test]
fn a_constant_domain_should_still_produce_a_tick() {
    assert_eq!(
        super::axes::nice_ticks(ValueDomain::new(5.0, 5.0)),
        vec![5.0]
    );
}

#[test]
fn a_step_should_round_up_to_one_two_or_five() {
    assert_eq!(nice_step(0.9), 1.0);
    assert_eq!(nice_step(1.1), 2.0);
    assert_eq!(nice_step(2.1), 5.0);
    assert_eq!(nice_step(6.0), 10.0);
    assert_eq!(nice_step(0.0), 1.0);
}

#[test]
fn a_single_point_should_be_centred_in_the_plot() {
    let layout = ChartLayout::new(bounds(), None, 60.0);

    assert_eq!(
        point_x(layout.plot, 1, 0),
        center(layout.plot).x,
        "one point is drawn in the middle"
    );
}

#[test]
fn the_first_and_last_points_should_keep_their_inset() {
    let layout = ChartLayout::new(bounds(), None, 60.0);

    assert_eq!(
        point_x(layout.plot, 10, 0),
        layout.plot.origin.x + DATA_EDGE_INSET
    );
    assert_eq!(
        point_x(layout.plot, 10, 9),
        layout.plot.right() - DATA_EDGE_INSET
    );
}

#[test]
fn the_nearest_point_should_clamp_to_the_ends() {
    let layout = ChartLayout::new(bounds(), None, 60.0);

    assert_eq!(nearest_index(layout.plot, 10, f32::MIN), Some(0));
    assert_eq!(nearest_index(layout.plot, 10, f32::MAX), Some(9));
    assert_eq!(nearest_index(layout.plot, 0, 10.0), None);
}

#[test]
fn the_value_axis_should_put_its_maximum_at_the_top() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let domain = ValueDomain::new(0.0, 100.0);

    assert_eq!(value_y(100.0, domain, layout.plot), layout.plot.origin.y);
    assert_eq!(value_y(0.0, domain, layout.plot), layout.plot.bottom());
    assert_eq!(
        value_y(50.0, domain, layout.plot),
        layout.plot.origin.y + layout.plot.size.height / 2.0
    );
}

#[test]
fn the_buy_half_of_a_bar_should_touch_the_sell_half() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let (buy_left, sell_left, width) = paired_bar_x_bounds(layout.plot, 6, 2);

    assert_eq!(buy_left + width, sell_left, "the halves meet at the point");
}

#[test]
fn bars_should_get_thinner_as_more_of_them_share_the_plot() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let (_, _, few) = paired_bar_x_bounds(layout.plot, 4, 0);
    let (_, _, many) = paired_bar_x_bounds(layout.plot, 30, 0);

    assert!(many <= few);
    assert!(many >= 2.0, "a bar stays visible");
}

#[test]
fn a_stacked_bar_should_always_fill_its_plot_height() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let (long, short) = stacked_bar_bounds(center(layout.plot).x, 10.0, 0.3, 0.7, layout.plot);

    assert_eq!(
        long.size.height + short.size.height,
        layout.plot.size.height
    );
    assert_eq!(short.origin.y, layout.plot.origin.y);
    assert_eq!(long.bottom(), layout.plot.bottom());
}

#[test]
fn a_stacked_bar_of_no_reading_should_still_be_a_whole_bar() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let (long, short) = stacked_bar_bounds(center(layout.plot).x, 10.0, 0.0, 0.0, layout.plot);

    assert_eq!(long.size.height, 0.0);
    assert_eq!(short.size.height, layout.plot.size.height);
}

#[test]
fn the_time_axis_should_always_label_both_ends() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let labels = position_time_ticks(
        label_indices(30, 5)
            .into_iter()
            .map(|index| (index, SmolStr::new_static("label"))),
        30,
        layout,
    );

    assert_eq!(
        labels.first().map(|tick| tick.label.as_str()),
        Some("label")
    );
    assert_eq!(labels.len(), 5);
    assert!(
        labels
            .iter()
            .all(|tick| tick.origin_x >= layout.time_axis.origin.x)
    );
}

#[test]
fn a_short_window_should_label_every_point_it_has() {
    assert_eq!(label_indices(3, 6), vec![0, 1, 2]);
}

#[test]
fn a_narrow_plot_should_label_fewer_intervals() {
    let wide = time_tick_indices(30, 1000.0);
    let narrow = time_tick_indices(30, 160.0);

    assert!(narrow.len() <= wide.len());
    assert!(narrow.len() >= 2, "both ends are still labelled");
}

#[test]
fn every_time_label_should_stay_inside_the_axis() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let ticks = position_time_ticks(
        label_indices(30, 6)
            .into_iter()
            .map(|index| (index, SmolStr::new_static("label"))),
        30,
        layout,
    );

    for tick in ticks {
        assert!(tick.origin_x >= layout.time_axis.origin.x);
        assert!(tick.origin_x + tick.width <= layout.time_axis.right() + f32::EPSILON);
    }
    assert_eq!(super::axes::TIME_TICK_WIDTH, 104.0);
}

#[test]
fn a_tooltip_should_flip_instead_of_leaving_the_plot() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let desired = Size::new(120.0, 80.0);
    let near_right = Point::new(layout.plot.right() - 2.0, layout.plot.origin.y + 10.0);
    let bounds = tooltip_bounds(near_right, desired, layout.plot, 10.0);

    assert!(bounds.right() <= layout.plot.right());
    assert!(bounds.origin.x < near_right.x, "the tooltip flipped left");
    assert!(bounds.origin.y >= layout.plot.origin.y);
}

#[test]
fn a_tooltip_bigger_than_its_plot_should_still_be_placed_inside_it() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let bounds = tooltip_bounds(
        center(layout.plot),
        Size::new(10_000.0, 10_000.0),
        layout.plot,
        10.0,
    );

    assert_eq!(bounds.size.width, layout.plot.size.width);
    assert_eq!(bounds.size.height, layout.plot.size.height);
    assert_eq!(bounds.origin, layout.plot.origin);
}

#[test]
fn a_y_axis_label_should_be_centred_on_the_line_it_names() {
    let layout = ChartLayout::new(bounds(), Some(60.0), 60.0);
    let y = layout.plot.origin.y + 100.0;

    assert_eq!(
        axis_label_y(y, layout.left_axis),
        y - AXIS_LINE_HEIGHT / 2.0
    );
}

#[test]
fn a_y_axis_label_at_the_edges_should_stay_inside_its_axis() {
    let layout = ChartLayout::new(bounds(), Some(60.0), 60.0);

    assert_eq!(
        axis_label_y(layout.left_axis.origin.y, layout.left_axis),
        layout.left_axis.origin.y
    );
    assert_eq!(
        axis_label_y(layout.left_axis.bottom(), layout.left_axis),
        layout.left_axis.bottom() - AXIS_LINE_HEIGHT
    );
}

#[test]
fn a_ratio_bar_should_stay_between_its_floor_and_its_ceiling() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let width = ratio_bar_width(layout.plot, 30);

    assert!((3.0..=18.0).contains(&width));
}

#[test]
fn the_buy_half_of_a_bar_should_start_at_its_point() {
    let layout = ChartLayout::new(bounds(), None, 60.0);
    let (buy, sell) = paired_bar_bounds(layout.plot, 4, 1, 2.0, 1.0, ValueDomain::new(0.0, 4.0));

    assert!(
        buy.origin.y < sell.origin.y,
        "the taller value is drawn higher"
    );
    assert_eq!(buy.size.width, sell.size.width);
}

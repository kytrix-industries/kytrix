use gpui_kit::{Bounds, Pixels, Point, point, px, size};
use smol_str::SmolStr;

use super::axes::layout::*;

fn bounds() -> Bounds<Pixels> {
    Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(400.0)))
}

fn center(plot: Bounds<Pixels>) -> Point<Pixels> {
    point(plot.origin.x + plot.size.width / 2.0, plot.origin.y)
}

#[test]
fn the_time_axis_should_sit_under_the_plot_and_the_legend_under_that() {
    let layout = ChartLayout::new(bounds(), Some(px(20.0)), px(20.0));

    assert_eq!(layout.time_axis.origin.y, layout.plot.bottom());
    assert_eq!(layout.legend.origin.y, layout.time_axis.bottom());
    assert!(layout.legend.bottom() <= layout.content.bottom());
}

#[test]
fn each_axis_should_share_the_plot_height_and_sit_at_its_own_edge() {
    let layout = ChartLayout::new(bounds(), Some(px(20.0)), px(20.0));

    assert_eq!(layout.left_axis.size.height, layout.plot.size.height);
    assert_eq!(layout.right_axis.size.height, layout.plot.size.height);
    assert_eq!(layout.left_axis.right(), layout.plot.origin.x);
    assert_eq!(layout.right_axis.origin.x, layout.plot.right());
}

#[test]
fn a_short_label_should_leave_its_axis_at_the_minimum_width() {
    let layout = ChartLayout::new(bounds(), Some(px(8.0)), px(8.0));

    assert_eq!(layout.left_axis.size.width, MINIMUM_AXIS_WIDTH);
    assert_eq!(layout.right_axis.size.width, MINIMUM_AXIS_WIDTH);
}

#[test]
fn a_long_label_should_widen_only_its_own_axis() {
    let layout = ChartLayout::new(bounds(), Some(px(80.0)), px(0.0));

    assert_eq!(layout.left_axis.size.width, px(92.0));
    assert_eq!(layout.right_axis.size.width, MINIMUM_AXIS_WIDTH);
}

#[test]
fn a_chart_without_a_left_axis_should_give_that_width_to_the_plot() {
    let layout = ChartLayout::new(bounds(), None, px(20.0));

    assert_eq!(layout.left_axis.size.width, Pixels::ZERO);
    assert_eq!(layout.plot.origin.x, layout.content.origin.x);
}

#[test]
fn a_narrow_pane_should_grow_the_content_instead_of_squeezing_the_plot() {
    let narrow = Bounds::new(point(px(0.0), px(0.0)), size(px(120.0), px(400.0)));
    let layout = ChartLayout::new(narrow, Some(px(200.0)), px(200.0));

    assert!(layout.plot.size.width > Pixels::ZERO);
    assert!(layout.content.size.width > narrow.size.width);
    assert!(layout.left_axis.size.width >= MINIMUM_AXIS_WIDTH);
    assert!(layout.right_axis.size.width >= MINIMUM_AXIS_WIDTH);
}

#[test]
fn a_pane_too_narrow_for_its_axes_should_still_keep_a_usable_plot() {
    // Each axis takes what it can and the content grows, rather than the plot
    // being crushed to nothing to make room for labels.
    let narrow = Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), px(400.0)));
    let layout = ChartLayout::new(narrow, Some(px(400.0)), px(400.0));

    assert!(layout.plot.size.width >= Pixels::from(1.0));
    assert!(layout.content.size.width >= narrow.size.width);
}

#[test]
fn a_short_pane_should_keep_the_time_axis_and_the_legend_inside_the_content() {
    let short = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(40.0)));
    let layout = ChartLayout::new(short, None, px(10.0));

    assert_eq!(layout.content.size.height, CHART_MIN_CONTENT_HEIGHT);
    assert!(layout.plot.size.height > Pixels::ZERO);
    assert!(layout.legend.bottom() <= layout.content.bottom());
}

#[test]
fn a_line_domain_should_keep_room_around_its_data() {
    let domain = line_domain([10.0, 20.0].into_iter());

    assert!(domain.minimum < 10.0);
    assert!(domain.maximum > 20.0);
    assert!(domain.minimum >= 0.0);
}

#[test]
fn a_line_domain_should_ignore_what_is_not_a_reading() {
    assert_eq!(
        line_domain([f64::NAN, f64::INFINITY, -5.0].into_iter()),
        ValueDomain::new(0.0, 1.0)
    );
    assert_eq!(line_domain([0.0].into_iter()), ValueDomain::new(0.0, 1.0));
}

#[test]
fn a_bar_domain_should_start_at_zero_and_leave_headroom() {
    let domain = bar_domain([4.0, 10.0].into_iter());

    assert_eq!(domain.minimum, 0.0);
    assert!(domain.maximum > 10.0);
}

#[test]
fn a_ratio_domain_should_stay_finite_for_a_constant_series() {
    let domain = ratio_domain([2.0, 2.0].into_iter());

    assert!(domain.minimum < 2.0);
    assert!(domain.maximum > 2.0);
    assert!(domain.minimum.is_finite() && domain.maximum.is_finite());
}

#[test]
fn a_ratio_domain_should_reject_values_that_are_not_ratios() {
    assert_eq!(
        ratio_domain([f64::NAN, 0.0, -1.0].into_iter()),
        ValueDomain::new(0.0, 1.0)
    );
}

#[test]
fn axis_ticks_should_be_ordered_finite_and_inside_the_domain() {
    let domain = ValueDomain::new(3.0, 97.0);
    let scale = AxisScale::new(domain, |value, _| SmolStr::new(value.to_string()));

    assert!(scale.ticks.len() >= 2);
    assert!(
        scale
            .ticks
            .windows(2)
            .all(|pair| pair[0].value < pair[1].value)
    );
    assert!(scale.ticks.iter().all(|tick| tick.value.is_finite()));
    assert!(scale.ticks.iter().all(|tick| {
        tick.value >= domain.minimum - 1e-9 && tick.value <= domain.maximum + 1e-9
    }));
}

#[test]
fn a_constant_domain_should_still_produce_a_tick() {
    let scale = AxisScale::new(ValueDomain::new(5.0, 5.0), |value, _| {
        SmolStr::new(value.to_string())
    });

    assert_eq!(scale.ticks.len(), 1);
}

#[test]
fn a_step_should_round_up_to_one_two_or_five() {
    assert_eq!(nice_step(0.9), 1.0);
    assert_eq!(nice_step(1.5), 2.0);
    assert_eq!(nice_step(3.0), 5.0);
    assert_eq!(nice_step(7.0), 10.0);
    assert_eq!(nice_step(f64::NAN), 1.0);
}

#[test]
fn a_single_point_should_be_centred_in_the_plot() {
    let plot = bounds();

    assert_eq!(point_x(plot, 1, 0), plot.origin.x + plot.size.width / 2.0);
}

#[test]
fn the_first_and_last_points_should_keep_their_inset() {
    let plot = bounds();
    let (left, right) = data_x_range(plot);

    assert_eq!(point_x(plot, 5, 0), left);
    assert_eq!(point_x(plot, 5, 4), right);
}

#[test]
fn the_nearest_point_should_clamp_to_the_ends() {
    let plot = bounds();

    assert_eq!(nearest_index(plot, 10, px(-100.0)), Some(0));
    assert_eq!(nearest_index(plot, 10, px(10_000.0)), Some(9));
    assert_eq!(nearest_index(plot, 0, px(10.0)), None);
}

#[test]
fn the_value_axis_should_put_its_maximum_at_the_top() {
    let plot = bounds();
    let domain = ValueDomain::new(0.0, 10.0);

    assert_eq!(value_y(0.0, domain, plot), plot.bottom());
    assert_eq!(value_y(10.0, domain, plot), plot.origin.y);
    assert_eq!(
        value_y(5.0, domain, plot),
        plot.origin.y + plot.size.height / 2.0
    );
}

#[test]
fn the_buy_half_of_a_bar_should_touch_the_sell_half() {
    let plot = bounds();
    let (buy, sell) = paired_bar_bounds(plot, 10, 0, 5.0, 5.0, ValueDomain::new(0.0, 10.0));

    assert!(buy.origin.x < sell.origin.x);
    assert_eq!(
        buy.right(),
        sell.origin.x,
        "one interval's two sides are drawn against each other, with no gap between them"
    );
    assert_eq!(buy.bottom(), plot.bottom());
}

#[test]
fn bars_should_get_thinner_as_more_of_them_share_the_plot() {
    let plot = bounds();
    let (_, _, wide) = paired_bar_x_bounds(plot, 5, 0);
    let (_, _, narrow) = paired_bar_x_bounds(plot, 30, 29);

    assert!(wide > narrow);
}

#[test]
fn a_stacked_bar_should_always_fill_its_plot_height() {
    let plot = bounds();
    let (long, short) = stacked_bar_bounds(center(plot).x, px(8.0), 0.6, 0.4, plot);

    assert_eq!(short.origin.y, plot.origin.y);
    assert_eq!(long.bottom(), plot.bottom());
    assert_eq!(long.size.height + short.size.height, plot.size.height);
}

#[test]
fn the_time_axis_should_always_label_both_ends() {
    let indices = label_indices(30, 5);

    assert_eq!(indices.first(), Some(&0));
    assert_eq!(indices.last(), Some(&29));
    assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn a_short_window_should_label_every_point_it_has() {
    assert_eq!(label_indices(3, 6), vec![0, 1, 2]);
    assert!(label_indices(0, 6).is_empty());
}

#[test]
fn a_narrow_plot_should_label_fewer_intervals() {
    let narrow = time_tick_indices(30, px(200.0));
    let wide = time_tick_indices(30, px(800.0));

    assert!(narrow.len() <= wide.len());
    assert!(narrow.len() >= 2);
}

#[test]
fn every_time_label_should_stay_inside_the_axis() {
    let layout = ChartLayout::new(bounds(), None, px(20.0));
    let ticks = position_time_ticks(
        label_indices(30, 5)
            .into_iter()
            .map(|index| (index, SmolStr::new(index.to_string()))),
        30,
        layout,
    );

    assert!(ticks.iter().all(|tick| {
        tick.origin_x >= layout.time_axis.origin.x
            && tick.origin_x + tick.width <= layout.time_axis.right() + px(0.5)
    }));
}

#[test]
fn a_tooltip_should_flip_instead_of_leaving_the_plot() {
    let plot = bounds();
    let desired = size(px(120.0), px(60.0));
    let gap = px(10.0);
    let at_the_right = tooltip_bounds(point(plot.right() - px(2.0), px(10.0)), desired, plot, gap);

    assert!(at_the_right.right() <= plot.right());
    assert!(at_the_right.origin.x < plot.right() - px(2.0));

    let at_the_bottom =
        tooltip_bounds(point(px(10.0), plot.bottom() - px(2.0)), desired, plot, gap);
    assert!(at_the_bottom.bottom() <= plot.bottom());
}

#[test]
fn a_tooltip_bigger_than_its_plot_should_still_be_placed_inside_it() {
    let plot = bounds();
    let placed = tooltip_bounds(center(plot), size(px(2_000.0), px(2_000.0)), plot, px(10.0));

    assert_eq!(placed.origin, plot.origin);
    assert_eq!(placed.size, plot.size);
}

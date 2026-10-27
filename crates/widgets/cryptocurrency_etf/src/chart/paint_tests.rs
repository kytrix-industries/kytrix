//! One prepared frame: the bars, the line and the days a window folds into, and the
//! lines a tooltip draws out of one of those days.
//!
//! The frame is what a resize costs, so what it is worth testing is that it is a
//! function of the layout alone: the same window in a wider box is more bars in the
//! same places, and a window of one day is a point rather than a line.

use base_ui::style::theme::{Theme, ThemeExt};
use gpui_kit::{Pixels, TestAppContext};
use smol_str::SmolStr;

use super::{
    axes::{HOVER_MARKER_SIZE, marker_bounds, point_x, value_y},
    paint::{CacheKey, Hover, PaintContext, PreparedFrame, measure, prepare, tooltip_lines},
    palette::AXIS_FONT_SIZE,
    test_support::{FUNDS, layout, prepared, prepared_all},
    text::ChartTextCache,
};
use crate::model::{EtfFundSelection, EtfWindow};

fn bar_values(prepared: &PreparedFrame) -> Vec<f64> {
    prepared.bars.iter().map(|bar| bar.value).collect()
}

/// Installs the translator, which is what the labels of a resolved view come from.
fn with_locales(cx: &mut TestAppContext, test: impl FnOnce(&mut gpui_kit::App)) {
    cx.update(|cx| {
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        test(cx);
    });
}

#[gpui_kit::test]
fn a_window_of_days_should_become_one_bar_each(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1.0, -2.0, 3.0]);
        let prepared = prepare(&data, layout(640.0, 360.0));

        assert_eq!(bar_values(&prepared), vec![1.0, -2.0, 3.0]);
        assert!(
            prepared
                .bars
                .iter()
                .all(|bar| bar.bounds.size.height > gpui_kit::px(0.0)),
            "every bar is drawn"
        );
        assert!(prepared.line.is_some(), "three days are a line");
        assert!(prepared.marker.is_none());
        assert!(!prepared.time_ticks.is_empty(), "the window is dated");
    });
}

#[gpui_kit::test]
fn a_window_of_one_day_should_be_a_point_rather_than_a_line(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[4.0]);
        let layout = layout(640.0, 360.0);
        let prepared = prepare(&data, layout);

        assert_eq!(prepared.bars.len(), 1);
        assert!(prepared.line.is_none(), "one day is not a line");
        let marker = prepared.marker.expect("the one reading is marked instead");
        assert_eq!(
            marker,
            gpui_kit::point(
                point_x(layout.plot, 1, 0),
                value_y(
                    4.0,
                    data.net_assets
                        .as_ref()
                        .expect("all funds have assets")
                        .domain,
                    layout.plot
                ),
            )
        );
    });
}

#[gpui_kit::test]
fn a_day_that_is_not_a_reading_should_draw_no_bar_and_break_the_line(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        // The third day is a row the service could not have published: it has no
        // flow, so it has no bar, and the line does not cross the gap as if the gap
        // were data.
        let data = prepared_all(cx, &[1.0, 2.0, f64::NAN, 4.0, 5.0]);
        let prepared = prepare(&data, layout(640.0, 360.0));

        assert_eq!(bar_values(&prepared), vec![1.0, 2.0, 4.0, 5.0]);
        assert!(
            prepared.line.is_some(),
            "the days either side of the gap are still a line"
        );
    });
}

#[gpui_kit::test]
fn the_bars_of_a_dense_window_should_be_prepared_as_one_layer(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let flows = vec![1.0; 400];
        let prepared = prepare(&prepared_all(cx, &flows), layout(640.0, 360.0));

        assert_eq!(prepared.bars.len(), 400);
        assert!(
            prepared.bars_are_disjoint,
            "a bar per day is a column of its own, so one layer can hold them all"
        );
        assert!(prepared.bars[0].bounds.right() <= prepared.bars[1].bounds.origin.x);
    });
}

#[gpui_kit::test]
fn a_named_fund_should_have_no_line_and_no_second_reading(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared(
            cx,
            &[1.0, 2.0],
            EtfFundSelection::Named(SmolStr::new_static(FUNDS[0])),
            EtfWindow::All,
        );
        let prepared = prepare(&data, layout(640.0, 360.0));

        assert!(!data.show_net_assets);
        assert!(data.net_assets.is_none());
        assert!(
            prepared.line.is_none() && prepared.marker.is_none(),
            "one fund's flow is a bar chart, not a level"
        );
    });
}

#[gpui_kit::test]
fn a_wider_box_should_spread_the_same_days_further_apart(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1.0; 30]);
        // The floor is what a narrow pane actually draws in: the content box has a
        // minimum width, so a pane narrower than the axes need scrolls instead of
        // squeezing them.
        let floor = layout(640.0, 360.0);
        let wide = layout(1_200.0, 360.0);
        let floor_prepared = prepare(&data, floor);
        let wide_prepared = prepare(&data, wide);

        assert_eq!(floor_prepared.bars.len(), wide_prepared.bars.len());
        assert!(
            wide_prepared.bars[29].bounds.origin.x > floor_prepared.bars[29].bounds.origin.x,
            "a wider plot places the last day further right"
        );
        assert!(floor_prepared.bars[0].bounds.origin.x >= floor.plot.origin.x);
        assert!(floor_prepared.bars[29].bounds.right() <= floor.plot.right());
        assert!(wide_prepared.bars[29].bounds.right() <= wide.plot.right());
        assert!(
            wide_prepared.bars[0].bounds.size.width <= gpui_kit::px(14.0),
            "a bar has a widest size, so few days do not become slabs"
        );
    });
}

#[test]
fn a_cache_key_should_name_the_box_and_the_window_it_was_prepared_from() {
    let key = CacheKey {
        bounds: super::test_support::bounds(640.0, 360.0),
        revision: 1,
    };
    let same = CacheKey {
        bounds: super::test_support::bounds(640.0, 360.0),
        revision: 1,
    };
    let resized = CacheKey {
        bounds: super::test_support::bounds(641.0, 360.0),
        revision: 1,
    };
    let redrawn = CacheKey {
        bounds: super::test_support::bounds(640.0, 360.0),
        revision: 2,
    };

    assert_eq!(key, same);
    assert_ne!(key, resized);
    assert_ne!(key, redrawn, "another window is another frame");
}

#[test]
fn a_hover_should_be_the_same_hover_until_a_reading_or_a_whole_pixel_changes() {
    let hover = Hover {
        index: 2,
        cursor_y: gpui_kit::px(40.0),
    };

    assert_eq!(
        hover,
        Hover {
            index: 2,
            cursor_y: gpui_kit::px(40.0)
        },
        "the same reading at the same height is not a change worth a frame"
    );
    assert_ne!(
        hover,
        Hover {
            index: 3,
            cursor_y: gpui_kit::px(40.0)
        }
    );
    assert_ne!(
        hover,
        Hover {
            index: 2,
            cursor_y: gpui_kit::px(41.0)
        }
    );
}

#[test]
fn a_hover_marker_should_be_centered_on_the_reading_it_marks() {
    let bounds = marker_bounds(gpui_kit::point(gpui_kit::px(100.0), gpui_kit::px(50.0)));

    assert_eq!(bounds.size.width, HOVER_MARKER_SIZE);
    assert_eq!(bounds.size.height, HOVER_MARKER_SIZE);
    assert_eq!(
        bounds.center(),
        gpui_kit::point(gpui_kit::px(100.0), gpui_kit::px(50.0))
    );
}

/// The tooltip is the one reading the chart draws in two colours, so which of the two
/// runs is coloured is worth pinning: the amount carries the direction the day went,
/// and the caption naming that direction is read like the rest of the tooltip.
#[gpui_kit::test]
fn a_tooltips_flow_should_colour_the_amount_and_not_the_caption(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1_000.0, -500.0]);
        let theme = Theme::default();
        let foreground = theme.components.tooltip.foreground.hsla;

        let gained = tooltip_lines(&data.points[0], true, &theme);
        assert_eq!(gained.len(), 3, "the date, the day's flow and the assets");
        assert_eq!(
            gained[1],
            vec![
                (data.points[0].flow_tooltip_caption.as_ref(), foreground),
                (
                    data.points[0].flow_label.as_ref(),
                    theme.components.chart.bullish.hsla
                ),
            ]
        );

        let lost = tooltip_lines(&data.points[1], true, &theme);
        assert_eq!(
            lost[1],
            vec![
                (data.points[1].flow_tooltip_caption.as_ref(), foreground),
                (
                    data.points[1].flow_label.as_ref(),
                    theme.components.chart.bearish.hsla
                ),
            ]
        );
    });
}

/// Each axis is measured against its title as well as against its ticks: the title is
/// where the unit of the bare numbers beside it comes from, and one wider than its axis
/// would be drawn off the edge of the chart and clipped away.
#[gpui_kit::test]
fn the_axes_should_be_measured_wide_enough_for_the_titles_they_draw(cx: &mut TestAppContext) {
    cx.update(|cx| {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
    });
    let cx = cx.add_empty_window();
    let mut cache = ChartTextCache::default();

    let (measurements, left_title, right_title) = cx.update(|window, cx| {
        let data = prepared_all(cx, &[1_000.0, -500.0]);
        let muted = cx.theme().components.chart.axis.hsla;
        let left_title = cache.width(data.left_axis_title.as_ref(), AXIS_FONT_SIZE, muted, window);
        let right_title = cache.width(
            data.right_axis_title.as_ref(),
            AXIS_FONT_SIZE,
            muted,
            window,
        );
        let measurements = measure(
            &data,
            &mut PaintContext {
                text_cache: &mut cache,
                window,
                app: cx,
            },
        );
        (measurements, left_title, right_title)
    });

    assert!(
        left_title > Pixels::ZERO && right_title > Pixels::ZERO,
        "a title is text, not a gap"
    );
    assert!(
        measurements.left_axis_width >= left_title,
        "the flow axis has to fit the title drawn above it"
    );
    assert!(
        measurements
            .right_axis_width
            .expect("the whole complex has a net-asset axis")
            >= right_title,
        "and so does the net-asset axis"
    );
}

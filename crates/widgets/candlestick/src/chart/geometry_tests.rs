//! The chart's box arithmetic: where every plot, axis and separator goes.

use gpui_kit::{Bounds, Pixels, point, px, size};
use market_participants_connector::catalog::test_support;
use market_participants_model::{
    decimal::{Price, Quantity},
    listing::venue::PriceBasis,
    market::{
        book::ListingCandlestick,
        price::MarketPrice,
        units::{MarketQuantity, QuantityUnit},
    },
};

use super::geometry::*;

const CHART: Bounds<Pixels> = Bounds {
    origin: point(px(0.0), px(0.0)),
    size: size(px(800.0), px(500.0)),
};

fn chart_layout(longest_label_width: Pixels, sub_plot_count: usize) -> ChartLayout {
    ChartLayout::new(
        CHART,
        LayoutInputs {
            longest_label_width,
            sub_plot_count,
        },
    )
}

fn layout_with_height(total: f32, sub_plot_count: usize) -> ChartLayout {
    ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(total))),
        LayoutInputs {
            longest_label_width: px(0.0),
            sub_plot_count,
        },
    )
}

fn assert_close(actual: f32, expected: f32, label: &str) {
    assert!(
        (actual - expected).abs() < 0.01,
        "{label}: expected {expected}, got {actual}"
    );
}

fn candle(timestamp: i64) -> ListingCandlestick {
    let value = Price::from(1_u64);
    let price = MarketPrice {
        value,
        quote_asset: test_support::currency("USD"),
        basis: PriceBasis::PerAssetUnit,
    };
    ListingCandlestick {
        listing: test_support::listing_key(&test_support::spot_route(), "BTCUSDT"),
        open: price.clone(),
        high: price.clone(),
        low: price.clone(),
        close: price,
        traded_quantity: Some(MarketQuantity {
            value: Quantity::from(1_u64),
            unit: QuantityUnit::Contracts,
        }),
        traded_notional: None,
        timestamp,
    }
}

#[test]
fn layout_should_place_a_sub_plot_below_the_price_plot() {
    let layout = chart_layout(px(0.0), 1);

    assert_eq!(
        layout.sub_plots[0].plot.origin.y,
        layout.price_plot.bottom() + px(PLOT_SEPARATOR_HEIGHT)
    );
}

#[test]
fn layout_should_place_time_axis_below_the_last_sub_plot() {
    let layout = chart_layout(px(0.0), SUB_PLOT_INLINE);

    assert_eq!(
        layout.time_axis.origin.y,
        layout.sub_plots.last().expect("sub plots").plot.bottom()
    );
}

#[test]
fn layout_should_align_each_y_axis_with_its_plot() {
    let layout = chart_layout(px(0.0), 2);

    assert_eq!(layout.price_axis.size.height, layout.price_plot.size.height);
    for sub_plot in &layout.sub_plots {
        assert_eq!(sub_plot.axis.size.height, sub_plot.plot.size.height);
    }
}

#[test]
fn layout_should_use_minimum_axis_width_for_short_labels() {
    let layout = chart_layout(px(8.0), 1);

    assert_eq!(layout.price_axis.size.width, px(MIN_AXIS_WIDTH));
}

#[test]
fn layout_should_expand_axis_for_long_labels() {
    let layout = chart_layout(px(80.0), 1);

    assert_eq!(layout.price_axis.size.width, px(92.0));
}

#[test]
fn layout_should_preserve_minimum_plot_width_when_label_is_too_wide() {
    let layout = ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(80.0))),
        LayoutInputs {
            longest_label_width: px(200.0),
            sub_plot_count: 1,
        },
    );

    assert_eq!(layout.price_plot.size.width, px(MIN_PLOT_WIDTH));
}

#[test]
fn layout_should_not_cap_fitting_labels_to_a_chart_ratio() {
    let layout = ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(300.0))),
        LayoutInputs {
            longest_label_width: px(160.0),
            sub_plot_count: 1,
        },
    );

    assert_eq!(layout.price_axis.size.width, px(172.0));
}

#[test]
fn layout_should_share_width_between_price_and_volume_axes() {
    let layout = chart_layout(px(80.0), 1);

    assert_eq!(
        layout.price_axis.size.width,
        layout.sub_plots[0].axis.size.width
    );
}

#[test]
fn layout_should_share_width_between_every_y_axis() {
    let layout = chart_layout(px(80.0), SUB_PLOT_INLINE);

    for sub_plot in &layout.sub_plots {
        assert_eq!(layout.price_axis.size.width, sub_plot.axis.size.width);
    }
}

#[test]
fn maximum_plot_width_should_assume_the_minimum_axis_width() {
    let layout = chart_layout(px(80.0), 1);

    assert_eq!(layout.maximum_plot_width(), px(758.0));
}

#[test]
fn hit_testing_should_accept_both_plots_and_reject_the_axes() {
    let layout = chart_layout(px(0.0), 2);

    assert!(layout.contains_data_point(&layout.price_plot.center()));
    for sub_plot in &layout.sub_plots {
        assert!(layout.contains_data_point(&sub_plot.plot.center()));
        assert!(!layout.contains_data_point(&sub_plot.axis.center()));
    }
    assert!(!layout.contains_data_point(&layout.price_axis.center()));
    assert!(!layout.contains_data_point(&layout.time_axis.center()));
}

#[test]
fn hit_testing_should_reject_a_separator() {
    let layout = chart_layout(px(0.0), 1);

    assert!(!layout.contains_data_point(&layout.separators[0].center()));
}

#[test]
fn candle_geometry_should_share_centers_between_price_and_volume_plots() {
    let layout = chart_layout(px(0.0), SUB_PLOT_INLINE);
    let price_geometry = CandleGeometry::new(layout.price_plot, 20, 10.0);

    for sub_plot in &layout.sub_plots {
        let sub_geometry = CandleGeometry::new(sub_plot.plot, 20, 10.0);
        assert_eq!(price_geometry.center_x(7), sub_geometry.center_x(7));
    }
}

#[test]
fn candle_geometry_should_share_body_width_between_price_and_volume_plots() {
    let layout = chart_layout(px(0.0), 1);
    let price_geometry = CandleGeometry::new(layout.price_plot, 20, 10.0);
    let sub_geometry = CandleGeometry::new(layout.sub_plots[0].plot, 20, 10.0);

    assert_eq!(price_geometry.body_width(), sub_geometry.body_width());
}

#[test]
fn candle_geometry_should_anchor_the_newest_candle_to_the_right_edge() {
    let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
    let geometry = CandleGeometry::new(plot, 10, 10.0);

    assert_eq!(geometry.center_x(9), px(95.0));
}

#[test]
fn candle_geometry_should_clamp_a_slot_outside_the_visible_range() {
    let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
    let geometry = CandleGeometry::new(plot, 10, 10.0);

    assert_eq!(geometry.slot_at(px(-50.0)), Some(0));
    assert_eq!(geometry.slot_at(px(500.0)), Some(9));
    assert_eq!(CandleGeometry::new(plot, 0, 10.0).slot_at(px(1.0)), None);
}

/// The height distribution table from the design's §6.2 (D17), row for row.
///
/// `(total, sub_count, time, content, sub_budget, per_sub, sub_total, price_height)`.
/// The values are what the formula produces, not estimates: `time` is the one
/// column that is `f32` (`min(26, total * 0.24)`), so 80 and 60 px panes carry
/// 19.2 and 14.4 rather than their integer truncations.
const HEIGHT_TABLE: [(f32, usize, f32, f32, f32, f32, f32, f32); 18] = [
    (500.0, 0, 26.0, 474.0, 0.0, 0.0, 0.0, 474.0),
    (500.0, 1, 26.0, 474.0, 113.76, 113.76, 114.76, 359.24),
    (500.0, 2, 26.0, 474.0, 227.52, 113.76, 229.52, 244.48),
    (500.0, 4, 26.0, 474.0, 284.4, 71.1, 288.4, 185.6),
    (240.0, 0, 26.0, 214.0, 0.0, 0.0, 0.0, 214.0),
    (240.0, 1, 26.0, 214.0, 51.36, 51.36, 52.36, 161.64),
    (240.0, 2, 26.0, 214.0, 102.72, 51.36, 104.72, 109.28),
    (240.0, 4, 26.0, 214.0, 128.4, 32.1, 132.4, 81.6),
    (120.0, 0, 26.0, 94.0, 0.0, 0.0, 0.0, 94.0),
    (120.0, 1, 26.0, 94.0, 22.56, 22.56, 23.56, 70.44),
    (120.0, 2, 26.0, 94.0, 44.0, 22.0, 46.0, 48.0),
    (120.0, 4, 26.0, 94.0, 42.0, 10.5, 46.0, 48.0),
    (80.0, 0, 19.2, 60.8, 0.0, 0.0, 0.0, 60.8),
    (80.0, 1, 19.2, 60.8, 11.8, 11.8, 12.8, 48.0),
    (80.0, 2, 19.2, 60.8, 10.8, 5.4, 12.8, 48.0),
    (80.0, 4, 19.2, 60.8, 8.8, 2.2, 12.8, 48.0),
    (60.0, 0, 14.4, 45.6, 0.0, 0.0, 0.0, 45.6),
    (60.0, 4, 14.4, 45.6, 0.0, 0.0, 0.0, 45.6),
];

#[test]
fn layout_should_match_the_height_table() {
    for (total, sub_count, time, content, sub_budget, per_sub, sub_total, price_height) in
        HEIGHT_TABLE
    {
        let label = format!("{total}px with {sub_count} sub plots");
        let layout = layout_with_height(total, sub_count);
        let actual_time = f32::from(layout.time_axis.size.height);
        let actual_price = f32::from(layout.price_plot.size.height);
        let actual_per_sub = layout
            .sub_plots
            .first()
            .map_or(0.0, |sub_plot| f32::from(sub_plot.plot.size.height));
        let actual_sub_total = layout
            .sub_plots
            .iter()
            .map(|sub_plot| f32::from(sub_plot.plot.size.height))
            .sum::<f32>()
            + layout
                .separators
                .iter()
                .map(|separator| f32::from(separator.size.height))
                .sum::<f32>();
        let actual_content = actual_price + actual_sub_total;

        assert_close(actual_time, time, &format!("{label}: time"));
        assert_close(actual_content, content, &format!("{label}: content"));
        // The ceiling is not reached by any row of the table, so the effective
        // budget is the per-sub height spread over the sub plots.
        assert_close(
            actual_per_sub * sub_count as f32,
            sub_budget,
            &format!("{label}: sub budget"),
        );
        assert_close(actual_per_sub, per_sub, &format!("{label}: per sub"));
        assert_close(actual_sub_total, sub_total, &format!("{label}: sub total"));
        assert_close(actual_price, price_height, &format!("{label}: price"));
    }
}

/// The ceiling is an upper bound on one sub plot, and the sub total is computed
/// from the capped height rather than the wish. A 1200px pane wishes for 281.76
/// per sub plot and must get 200.
#[test]
fn layout_should_cap_a_sub_plot_at_the_ceiling() {
    let layout = layout_with_height(1200.0, 1);

    assert_close(
        f32::from(layout.sub_plots[0].plot.size.height),
        SUB_PLOT_HEIGHT_CEILING,
        "per sub",
    );
    assert_close(
        f32::from(layout.sub_plots[0].plot.size.height)
            + f32::from(layout.separators[0].size.height),
        SUB_PLOT_HEIGHT_CEILING + PLOT_SEPARATOR_HEIGHT,
        "sub total",
    );
    assert_close(
        f32::from(layout.price_plot.size.height),
        973.0,
        "price height",
    );
}

/// The three invariants from the design's §6.2, over the whole input space the
/// callers use — including the `content == 0` pane at 0px, where the formula is
/// most likely to divide by zero or go negative.
#[test]
fn layout_should_preserve_the_height_invariants() {
    const HEIGHTS: [f32; 8] = [0.0, 40.0, 60.0, 80.0, 120.0, 240.0, 500.0, 1200.0];

    for total in HEIGHTS {
        for sub_plot_count in 0..=SUB_PLOT_INLINE {
            let label = format!("{total}px with {sub_plot_count} sub plots");
            let layout = layout_with_height(total, sub_plot_count);
            let time_height = f32::from(layout.time_axis.size.height);
            let content = (total - time_height).max(0.0);
            let price_height = f32::from(layout.price_plot.size.height);
            let per_sub = layout
                .sub_plots
                .first()
                .map_or(0.0, |sub_plot| f32::from(sub_plot.plot.size.height));
            let sub_total = layout
                .sub_plots
                .iter()
                .map(|sub_plot| f32::from(sub_plot.plot.size.height))
                .sum::<f32>()
                + layout
                    .separators
                    .iter()
                    .map(|separator| f32::from(separator.size.height))
                    .sum::<f32>();

            assert!(
                price_height >= f32::min(MIN_PLOT_HEIGHT, content) - 0.01,
                "{label}: the price plot must keep its floor or take the whole content \
                 (price {price_height}, content {content})"
            );
            assert!(
                (time_height + sub_total + price_height - total).abs() < 0.01,
                "{label}: the boxes must add up to the pane (time {time_height}, \
                 sub total {sub_total}, price {price_height}, total {total})"
            );
            assert!(
                price_height >= 0.0 && per_sub >= 0.0 && sub_total >= 0.0,
                "{label}: no box may be negative (price {price_height}, per sub {per_sub}, \
                 sub total {sub_total})"
            );
        }
    }
}

#[test]
fn layout_should_draw_no_sub_plots_when_the_price_floor_takes_the_content() {
    let layout = layout_with_height(60.0, SUB_PLOT_INLINE);

    assert!(layout.sub_plots.is_empty());
    assert!(layout.separators.is_empty());
    assert_eq!(layout.time_axis.origin.y, layout.price_plot.bottom());
    assert_close(
        f32::from(layout.price_plot.size.height),
        45.6,
        "price height",
    );
}

#[test]
fn every_sub_plot_should_have_one_separator_above_it() {
    const HEIGHTS: [f32; 6] = [0.0, 60.0, 80.0, 120.0, 500.0, 1200.0];

    for total in HEIGHTS {
        for sub_plot_count in 0..=SUB_PLOT_INLINE {
            let layout = layout_with_height(total, sub_plot_count);

            assert_eq!(
                layout.separators.len(),
                layout.sub_plots.len(),
                "{total}px with {sub_plot_count} sub plots"
            );
            for (separator, sub_plot) in layout.separators.iter().zip(&layout.sub_plots) {
                assert_eq!(separator.bottom(), sub_plot.plot.origin.y);
            }
        }
    }
}

#[test]
fn the_time_axis_should_always_sit_below_the_last_sub_plot() {
    const HEIGHTS: [f32; 6] = [0.0, 60.0, 80.0, 120.0, 500.0, 1200.0];

    for total in HEIGHTS {
        for sub_plot_count in 0..=SUB_PLOT_INLINE {
            let layout = layout_with_height(total, sub_plot_count);
            let last_plot_bottom = layout
                .sub_plots
                .last()
                .map_or(layout.price_plot.bottom(), |sub_plot| {
                    sub_plot.plot.bottom()
                });

            assert_eq!(
                layout.time_axis.origin.y, last_plot_bottom,
                "{total}px with {sub_plot_count} sub plots"
            );
        }
    }
}

#[test]
fn data_bounds_should_cover_the_price_plot_and_every_sub_plot() {
    let without_sub_plots = chart_layout(px(0.0), 0);
    assert_eq!(
        without_sub_plots.data_bounds(),
        without_sub_plots.price_plot
    );

    let with_sub_plots = chart_layout(px(0.0), SUB_PLOT_INLINE);
    let bounds = with_sub_plots.data_bounds();

    assert_eq!(bounds.origin, with_sub_plots.price_plot.origin);
    assert_eq!(bounds.size.width, with_sub_plots.price_plot.size.width);
    assert_eq!(
        bounds.bottom(),
        with_sub_plots
            .sub_plots
            .last()
            .expect("sub plots")
            .plot
            .bottom()
    );
}

#[test]
fn visible_range_should_follow_timestamp_anchor() {
    let candles = (1..=10).map(candle).collect::<Vec<_>>();
    let viewport = ChartViewport {
        candle_width: 10.0,
        right_timestamp: Some(6),
        cursor: None,
    };

    assert_eq!(visible_range(&candles, viewport, px(30.0)), 2..6);
}

#[test]
fn visible_range_should_show_the_newest_candles_when_following_the_live_edge() {
    let candles = (1..=10).map(candle).collect::<Vec<_>>();
    let viewport = ChartViewport {
        candle_width: 10.0,
        right_timestamp: None,
        cursor: None,
    };

    assert_eq!(visible_range(&candles, viewport, px(30.0)), 6..10);
}

#[test]
fn visible_range_should_be_empty_without_candles() {
    let viewport = ChartViewport::default();

    assert_eq!(visible_range(&[], viewport, px(300.0)), 0..0);
}

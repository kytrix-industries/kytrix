//! What an open-interest tooltip says, the one thing it does not do, and what its
//! hover marks.
//!
//! The chart is the exception to the rule the other two follow — a tooltip line is
//! read in the colour of the series it names — so the exception is what is pinned
//! here, together with the thing it is not an exception to: a hover marks each line
//! in the colour that line's own legend entry names. Nothing else about this chart
//! is decided in this module: the geometry is `paint`'s, and the boxes it is drawn
//! in are `axes`'.

use std::sync::Arc;

use gpui_kit::{Bounds, TestAppContext, point, px, size};
use market_participants_model::decimal::Quantity;

use super::{
    axes::ChartLayout,
    canvas::ChartData,
    open_interest::OpenInterestChart,
    paint::{Frame, Hover, LegendMeasurements, hover_markers},
};
use crate::model::OpenInterestPoint;

fn quantity(value: &str) -> Quantity {
    value.parse().expect("the fixture quantity should parse")
}

/// Three intervals of one listing, measured in both of its assets.
fn points() -> Arc<[OpenInterestPoint]> {
    [(10_u32, 1_000_u32), (20, 2_000), (30, 3_000)]
        .into_iter()
        .enumerate()
        .map(|(index, (base, quote))| OpenInterestPoint {
            timestamp: 1_700_000_000_000 + index as i64 * 300_000,
            base_asset_volume: quantity(&base.to_string()),
            quote_asset_volume: Some(quantity(&quote.to_string())),
        })
        .collect::<Vec<_>>()
        .into()
}

#[gpui_kit::test]
fn an_open_interest_tooltip_should_read_every_line_in_one_colour(cx: &mut TestAppContext) {
    cx.update(|cx| {
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
    });
    let mut chart = cx.update(|cx| OpenInterestChart::new(cx, "BTC", "USDT"));
    assert!(
        chart.set_snapshot(1, None, points()),
        "the first window is always a change"
    );

    let tooltip = chart
        .tooltip(1)
        .expect("every point of the window is annotated");

    assert_eq!(
        tooltip.entries.len(),
        2,
        "the tooltip names both readings of the interval"
    );
    assert!(
        tooltip.entries.iter().all(|entry| entry.color.is_none()),
        "the two lines are one instrument measured two ways, so neither is read in a series colour: \
         both are drawn in the tooltip's own foreground"
    );
}

#[gpui_kit::test]
fn an_open_interest_hover_should_mark_each_line_in_its_own_legend_colour(cx: &mut TestAppContext) {
    cx.update(|cx| {
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
    });
    let mut chart = cx.update(|cx| OpenInterestChart::new(cx, "BTC", "USDT"));
    assert!(
        chart.set_snapshot(1, None, points()),
        "the first window is always a change"
    );
    let layout = ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(400.0))),
        Some(px(60.0)),
        px(60.0),
    );
    let prepared = chart.prepare(layout);

    cx.update(|cx| {
        let legend = chart.legend(cx);
        let (left_title, right_title) = chart.axis_titles(cx);
        let frame = Frame {
            layout,
            scales: chart.scales(),
            axis_titles: (&left_title, &right_title),
            legend: &legend,
            legend_measurements: LegendMeasurements::default(),
        };
        let marked = hover_markers(
            &prepared,
            Hover {
                index: 1,
                cursor: layout.plot.center(),
            },
            &frame,
        )
        .into_iter()
        .map(|marker| marker.series)
        .collect::<Vec<_>>();

        assert_eq!(
            marked,
            legend.iter().map(|entry| entry.series).collect::<Vec<_>>(),
            "every line a hover marks is marked in the colour its own legend entry names, \
             not in the colour of the rule that crosses it"
        );
    });
}

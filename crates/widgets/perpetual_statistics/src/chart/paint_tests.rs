//! What a frame marks and where it puts its labels.
//!
//! The two things worth pinning here are the ones a reader notices immediately when
//! they are wrong: which points a hover marks, and where a y-axis label sits
//! relative to the grid line it names.

use gpui_kit::{Bounds, SharedString, point, px, size};
use market_participants_model::decimal::Quantity;
use smol_str::SmolStr;

use super::paint::*;
use crate::{
    chart::{
        axes::{AxisScale, ChartLayout, axis_label_y, bar_domain, line_domain, value_y},
        palette::{AXIS_LINE_HEIGHT, HOVER_MARKER_SIZE},
    },
    model::{OpenInterestPoint, TakerVolumePoint, quantity_as_f64},
};

fn layout() -> ChartLayout {
    ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(400.0))),
        Some(px(60.0)),
        px(60.0),
    )
}

fn quantity(value: &str) -> Quantity {
    value.parse().expect("the fixture quantity should parse")
}

/// A window whose two series are far apart, so a marker for each is a marker at a
/// different height.
///
/// The second interval is deliberately not the midpoint of either series: two
/// linear series would put both of their markers at the same fraction of their own
/// axis, and a test could not tell one marker from two.
fn points() -> Vec<OpenInterestPoint> {
    [(10_u32, 1_000_u32), (20, 2_000), (100, 3_000)]
        .into_iter()
        .enumerate()
        .map(|(index, (base, quote))| OpenInterestPoint {
            timestamp: 1_700_000_000_000 + index as i64 * 300_000,
            base_asset_volume: quantity(&base.to_string()),
            quote_asset_volume: Some(quantity(&quote.to_string())),
        })
        .collect()
}

fn scales(points: &[OpenInterestPoint]) -> (AxisScale, AxisScale) {
    let amount = AxisScale::new(
        line_domain(
            points
                .iter()
                .map(|point| quantity_as_f64(point.base_asset_volume)),
        ),
        |value, _| SmolStr::new(format!("{value}")),
    );
    let value = AxisScale::new(
        line_domain(
            points
                .iter()
                .filter_map(|point| point.quote_asset_volume)
                .map(quantity_as_f64),
        ),
        |value, _| SmolStr::new(format!("{value}")),
    );
    (amount, value)
}

/// Three intervals of taker volume, whose buy and sell sides are read at their own
/// heights.
fn volume_points() -> Vec<TakerVolumePoint> {
    [(10_u32, 30_u32), (20, 15), (30, 5)]
        .into_iter()
        .enumerate()
        .map(|(index, (buy, sell))| TakerVolumePoint {
            timestamp: 1_700_000_000_000 + index as i64 * 300_000,
            buy_volume: quantity(&buy.to_string()),
            sell_volume: quantity(&sell.to_string()),
        })
        .collect()
}

fn volume_scale(points: &[TakerVolumePoint]) -> AxisScale {
    AxisScale::new(
        bar_domain(points.iter().flat_map(|point| {
            [
                quantity_as_f64(point.buy_volume),
                quantity_as_f64(point.sell_volume),
            ]
        })),
        |value, _| SmolStr::new(format!("{value}")),
    )
}

#[test]
fn a_hover_should_mark_every_series_at_its_own_reading() {
    let points = points();
    let layout = layout();
    let (amount, value) = scales(&points);
    let prepared = prepare_open_interest(&points, &amount, &value, &[], layout);
    let left_title: Option<SharedString> = None;
    let right_title = SharedString::default();
    let axis_titles = (&left_title, &right_title);
    let frame = Frame {
        layout,
        scales: (Some(&amount), &value, None),
        axis_titles,
        legend: &[],
        legend_measurements: LegendMeasurements::default(),
    };

    let markers = hover_markers(
        &prepared,
        Hover {
            index: 1,
            cursor: layout.plot.center(),
        },
        &frame,
    );

    assert_eq!(
        markers.len(),
        2,
        "both lines are read at the hovered interval, so both are marked"
    );
    assert_ne!(
        markers[0].series, markers[1].series,
        "each marker names the line it sits on, so the two mark different series"
    );
    assert_eq!(
        markers[0].bounds.center().x,
        markers[1].bounds.center().x,
        "the two readings are of one interval, so they share an x"
    );
    assert_ne!(
        markers[0].bounds.center().y,
        markers[1].bounds.center().y,
        "each reading is against its own axis, so it has its own y"
    );
    for marker in markers {
        assert_eq!(marker.bounds.size.width, px(HOVER_MARKER_SIZE));
        assert!(layout.plot.contains(&marker.bounds.center()));
    }
}

#[test]
fn a_hover_should_mark_no_point_of_a_paired_bar_chart() {
    let points = volume_points();
    let volume = volume_scale(&points);
    let layout = layout();
    let prepared = prepare_taker_volume(&points, &volume, &[], layout);
    let left_title: Option<SharedString> = None;
    let right_title = SharedString::default();
    let frame = Frame {
        layout,
        scales: (None, &volume, None),
        axis_titles: (&left_title, &right_title),
        legend: &[],
        legend_measurements: LegendMeasurements::default(),
    };

    let markers = hover_markers(
        &prepared,
        Hover {
            index: 1,
            cursor: layout.plot.center(),
        },
        &frame,
    );

    assert!(
        markers.is_empty(),
        "a bar's top is the bar's own edge, so the rule through the interval marks it"
    );
}

#[test]
fn a_y_axis_label_should_be_centred_on_the_line_it_names() {
    let plot = layout().plot;
    let axis = layout().left_axis;
    let domain = line_domain([10.0_f64, 30.0].into_iter());
    let y = value_y(20.0, domain, plot);

    let label = axis_label_y(y, axis);

    assert_eq!(
        label + px(AXIS_LINE_HEIGHT) / 2.0,
        y,
        "a label belongs beside its own grid line, not below it"
    );
}

#[test]
fn a_y_axis_label_at_the_edges_should_stay_inside_its_axis() {
    let plot = layout().plot;
    let axis = layout().left_axis;

    assert_eq!(
        axis_label_y(plot.origin.y, axis),
        axis.origin.y,
        "the topmost label is pushed down rather than half outside the axis"
    );
    assert_eq!(
        axis_label_y(plot.bottom(), axis),
        axis.bottom() - px(AXIS_LINE_HEIGHT),
        "and the bottom one is pushed up"
    );
}

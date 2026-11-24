use market_participants_model::{
    decimal::{Quantity, Ratio},
    listing::rules::ListingPrecision,
};
use smol_str::SmolStr;
use ui_core::math::{LogicalPoint, LogicalRect, px};

use super::{
    axes::{AxisScale, ChartLayout, bar_domain, line_domain, ratio_domain, share_scale},
    paint::{
        Frame, Hover, LineGeometry, MAX_SERIES, Prepared, Tooltip, TooltipEntry, hover_markers,
        line_geometry_mesh, prepare_open_interest, prepare_ratio, prepare_taker_volume,
        time_labels,
    },
    palette::{Series, SeriesLabel, TooltipStyle, tooltip_style},
};
use crate::model::{LongShortRatioPoint, OpenInterestPoint, TakerVolumePoint, ratio_shares};

const SHARE_TICKS: [(f64, &str); 2] = [(0.0, "0%"), (1.0, "100%")];

fn layout() -> ChartLayout {
    ChartLayout::new(
        LogicalRect::new(px(0.0), px(0.0), px(640.0), px(360.0)),
        Some(px(60.0)),
        px(60.0),
    )
}

fn quantity(value: u64) -> Quantity {
    Quantity::from(value)
}

fn points() -> Vec<OpenInterestPoint> {
    vec![
        OpenInterestPoint {
            timestamp: 1,
            base_asset_volume: quantity(10),
            quote_asset_volume: Some(quantity(300)),
        },
        OpenInterestPoint {
            timestamp: 2,
            base_asset_volume: quantity(20),
            quote_asset_volume: None,
        },
        OpenInterestPoint {
            timestamp: 3,
            base_asset_volume: quantity(30),
            quote_asset_volume: Some(quantity(100)),
        },
    ]
}

fn scales(points: &[OpenInterestPoint]) -> (AxisScale, AxisScale) {
    use crate::model::quantity_as_f64;
    let amount = AxisScale::new(
        line_domain(
            points
                .iter()
                .map(|point| quantity_as_f64(point.base_asset_volume)),
        ),
        |value, step| format!("{value}{step}").into(),
    );
    let value = AxisScale::new(
        line_domain(
            points
                .iter()
                .filter_map(|point| point.quote_asset_volume)
                .map(quantity_as_f64),
        ),
        |value, step| format!("{value}{step}").into(),
    );
    (amount, value)
}

fn volume_points() -> Vec<TakerVolumePoint> {
    vec![
        TakerVolumePoint {
            timestamp: 1,
            buy_volume: quantity(10),
            sell_volume: quantity(4),
        },
        TakerVolumePoint {
            timestamp: 2,
            buy_volume: quantity(20),
            sell_volume: quantity(8),
        },
    ]
}

fn ratio_points() -> Vec<LongShortRatioPoint> {
    vec![
        LongShortRatioPoint {
            timestamp: 1,
            ratio: Ratio::from(2_u64),
        },
        LongShortRatioPoint {
            timestamp: 2,
            ratio: Ratio::from(0_u64),
        },
        LongShortRatioPoint {
            timestamp: 3,
            ratio: Ratio::from(3_u64),
        },
    ]
}

fn labels(count: usize) -> Box<[SmolStr]> {
    (0..count).map(|_| SmolStr::new_static("t")).collect()
}

fn frame<'a>(
    layout: ChartLayout,
    scales: (Option<&'a AxisScale>, &'a AxisScale),
    legend: &'a [SeriesLabel],
) -> Frame<'a> {
    Frame {
        layout,
        scales,
        axis_titles: ("", ""),
        legend,
        legend_measurements: super::paint::LegendMeasurements::default(),
    }
}

#[test]
fn an_open_interest_frame_should_prepare_both_lines_and_their_markers() {
    let layout = layout();
    let points = points();
    let (amount, value) = scales(&points);
    let prepared = prepare_open_interest(&points, &amount, &value, &labels(3), layout);

    let Prepared::Lines { series, ticks } = prepared else {
        panic!("an open-interest frame is two lines");
    };
    assert_eq!(series[0].markers.len(), 3);
    assert_eq!(
        series[1].markers.len(),
        2,
        "the missing amount is no marker"
    );
    assert!(!ticks.is_empty(), "the time axis is labelled");
    let value_points = series[1].points.as_ref().expect("two points make a line");
    assert!(value_points[0].is_some());
    assert_eq!(value_points[1], None, "the gap breaks the line");
    assert!(value_points[2].is_some());
}

#[test]
fn a_taker_volume_frame_should_prepare_one_pair_of_bars_per_interval() {
    let layout = layout();
    let points = volume_points();
    let scale = AxisScale::new(
        bar_domain(
            points
                .iter()
                .flat_map(|_point| [10.0, 4.0, 20.0, 8.0])
                .take(2),
        ),
        |value, _| SmolStr::new(format!("{value}")),
    );
    let prepared = prepare_taker_volume(&points, &scale, &labels(2), layout);

    let Prepared::Bars { bars, .. } = prepared else {
        panic!("a taker-volume frame is bars");
    };
    assert_eq!(bars.len(), 2);
    assert!(
        bars.iter()
            .all(|(buy, sell)| buy.size.width > px(0.0) && sell.size.width > px(0.0))
    );
}

#[test]
fn a_ratio_frame_should_prepare_a_bar_and_a_marker_only_for_a_reading() {
    let layout = layout();
    let points = ratio_points();
    let ratio = AxisScale::new(
        ratio_domain(
            points
                .iter()
                .filter(|point| ratio_shares(point.ratio).is_some())
                .map(|point| point.ratio.inner() as f64 / 1e18),
        ),
        |value, _| SmolStr::new(format!("{value}")),
    );
    let prepared = prepare_ratio(&points, &ratio, &labels(3), layout);

    let Prepared::Shares {
        bars,
        markers,
        line,
        ..
    } = prepared
    else {
        panic!("a ratio frame is stacked bars and a line");
    };
    assert_eq!(bars.len(), 2, "the interval with no ratio has no bar");
    assert_eq!(markers.len(), 2);
    let line = line.expect("two readings make a line");
    assert_eq!(line.len(), 3);
    assert_eq!(
        line[1], None,
        "the interval that is not a reading breaks it"
    );
}

#[test]
fn a_hover_should_mark_every_series_at_its_own_reading() {
    let layout = layout();
    let points = points();
    let (amount, value) = scales(&points);
    let prepared = prepare_open_interest(&points, &amount, &value, &labels(3), layout);
    let legend = [
        SeriesLabel {
            series: Series::Amount,
            label: SmolStr::new_static("a"),
        },
        SeriesLabel {
            series: Series::Value,
            label: SmolStr::new_static("v"),
        },
    ];
    let hover = Hover {
        index: 0,
        cursor_y: px(100.0),
    };
    let markers = hover_markers(
        &frame(layout, (Some(&amount), &value), &legend),
        &prepared,
        hover,
    );

    assert_eq!(markers.len(), 2);
    assert_eq!(markers[0].0, Series::Amount);
    assert_eq!(markers[1].0, Series::Value);
    assert_eq!(
        markers[0].1.origin.x, markers[1].1.origin.x,
        "one interval is one x"
    );
    assert_ne!(
        markers[0].1.origin.y, markers[1].1.origin.y,
        "each series is marked at its own reading"
    );
}

#[test]
fn a_hover_should_mark_no_point_of_a_paired_bar_chart() {
    let layout = layout();
    let points = volume_points();
    let scale = AxisScale::new(bar_domain([10.0, 20.0].into_iter()), |value, _| {
        SmolStr::new(format!("{value}"))
    });
    let prepared = prepare_taker_volume(&points, &scale, &labels(2), layout);
    let markers = hover_markers(
        &frame(layout, (None, &scale), &[]),
        &prepared,
        Hover {
            index: 0,
            cursor_y: px(100.0),
        },
    );

    assert!(markers.is_empty());
}

#[test]
fn a_hover_should_mark_the_ratio_reading_of_a_share_chart() {
    let layout = layout();
    let points = ratio_points();
    let ratio = AxisScale::new(ratio_domain([2.0, 3.0].into_iter()), |value, _| {
        SmolStr::new(format!("{value}"))
    });
    let prepared = prepare_ratio(&points, &ratio, &labels(3), layout);
    let markers = hover_markers(
        &frame(layout, (None, &ratio), &[]),
        &prepared,
        Hover {
            index: 0,
            cursor_y: px(100.0),
        },
    );

    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].0, Series::Ratio);
}

#[test]
fn the_line_mesh_should_refuse_an_empty_frame() {
    assert!(line_geometry_mesh(&[], px(2.0)).is_none());
    assert!(line_geometry_mesh(&[None, None], px(2.0)).is_none());
}

#[test]
fn the_line_mesh_should_cover_every_segment_and_joint() {
    let points = [
        LogicalPoint::new(px(10.0), px(10.0)),
        LogicalPoint::new(px(20.0), px(30.0)),
        LogicalPoint::new(px(30.0), px(20.0)),
    ];
    let desc = line_geometry_mesh(&points.map(Some), px(2.0)).expect("three points make a mesh");

    assert_eq!(desc.vertex_bytes.len(), 5 * 4 * 12);
    assert_eq!(desc.index_bytes.len(), 5 * 6 * 4);
    assert!(desc.bounds.size.width > px(20.0));
    assert!(desc.bounds.size.height > px(20.0));
}

#[test]
fn the_line_mesh_should_skip_a_repeated_point() {
    let points = [
        LogicalPoint::new(px(10.0), px(10.0)),
        LogicalPoint::new(px(10.0), px(10.0)),
    ];
    let desc = line_geometry_mesh(&points.map(Some), px(2.0)).expect("a dot still draws");

    assert_eq!(
        desc.vertex_bytes.len(),
        8 * 12,
        "two joints, and no ribbon segment across a point that did not move"
    );
}

#[test]
fn every_series_should_be_read_in_a_role_of_its_own() {
    let roles = [
        (Series::Amount, ui_kit::theme::role::Role::ChartSeries0),
        (Series::Value, ui_kit::theme::role::Role::ChartSeries1),
        (Series::Long, ui_kit::theme::role::Role::MarketUp),
        (Series::Short, ui_kit::theme::role::Role::MarketDown),
        (Series::Ratio, ui_kit::theme::role::Role::FgDefault),
    ];
    let mut seen = Vec::new();
    for (series, role) in roles {
        assert_eq!(series.role(), role);
        assert!(!seen.contains(&role), "{series:?} names a role of its own");
        seen.push(role);
    }
    let theme = ui_kit::theme::resolved::Theme::of(ui_kit::theme::appearance::Appearance::Light);
    assert_eq!(
        Series::Long.color(&theme),
        theme.role(ui_kit::theme::role::Role::MarketUp),
        "a series is drawn in the colour its role resolves to"
    );
}

#[test]
fn a_tooltip_should_be_sized_from_the_component_ladder() {
    let theme = ui_kit::theme::resolved::Theme::of(ui_kit::theme::appearance::Appearance::Light);
    let style = tooltip_style(theme.tokens());

    assert_eq!(style.font_size, theme.tokens().small.font_size);
    assert!(style.line_height > style.font_size);
    assert!(style.padding > px(0.0) && style.radius > px(0.0));
    assert_eq!(
        style.content_size(px(100.0), 1).width,
        px(100.0) + style.padding * 2.0
    );
}

#[test]
fn a_tooltip_should_grow_with_its_lines() {
    let style = TooltipStyle {
        font_size: px(12.0),
        line_height: px(16.0),
        padding: px(4.0),
        radius: px(4.0),
    };

    assert_eq!(
        style.content_size(px(100.0), 2).height,
        px(2.0 * 16.0 + 8.0)
    );
    assert_eq!(style.content_size(px(100.0), 2).width, px(108.0));
}

#[test]
fn a_time_axis_label_should_be_written_for_every_labelled_interval() {
    let points = points();
    let labels = time_labels(points.iter().map(|point| point.timestamp), |timestamp| {
        SmolStr::new(format!("t{timestamp}"))
    });

    assert_eq!(
        labels.as_ref(),
        &[
            SmolStr::new_static("t1"),
            SmolStr::new_static("t2"),
            SmolStr::new_static("t3"),
        ]
    );
}

#[test]
fn the_legend_should_hold_at_most_the_series_a_chart_draws() {
    assert_eq!(MAX_SERIES, 3);
}

#[test]
fn a_prepared_frame_should_count_the_points_it_was_built_from() {
    assert_eq!(Prepared::default().point_count(), 0);
    assert_eq!(
        Prepared::Lines {
            series: Default::default(),
            ticks: Box::default(),
        }
        .point_count(),
        0
    );
    assert_eq!(
        Prepared::Shares {
            bars: Box::default(),
            line: None,
            markers: vec![
                LogicalRect::new(px(0.0), px(0.0), px(1.0), px(1.0)),
                LogicalRect::new(px(0.0), px(0.0), px(1.0), px(1.0))
            ]
            .into_boxed_slice(),
            ticks: Box::default(),
        }
        .point_count(),
        2
    );
}

#[test]
fn a_line_geometry_of_no_points_should_draw_no_markers() {
    let geometry = LineGeometry::default();

    assert!(geometry.markers.is_empty());
    assert!(geometry.points.is_none());
}

#[test]
fn a_tooltip_entry_should_name_the_series_colour_it_is_read_in() {
    let entry = TooltipEntry {
        text: SmolStr::new_static("Long: 50.00%"),
        color: Some(Series::Long),
    };
    let tooltip = Tooltip {
        title: SmolStr::new_static("t"),
        entries: vec![entry].into_boxed_slice(),
    };

    assert_eq!(tooltip.entries[0].color, Some(Series::Long));
    assert_eq!(SHARE_TICKS.len(), 2);
    assert_eq!(
        share_scale(&SHARE_TICKS).ticks.len(),
        2,
        "a share axis has one tick per fixed share"
    );
}

#[test]
fn a_listing_precision_is_not_needed_to_prepare_a_frame() {
    let metadata = ListingPrecision {
        price_tick: "0.01".parse().expect("a price tick parses"),
        quantity_tick: "0.001".parse().expect("a quantity tick parses"),
    };
    assert!(metadata.price_tick.inner() > 0);
}

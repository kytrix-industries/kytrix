use base_ui::theme::{appearance::Appearance, resolved::Theme};
use smol_str::SmolStr;
use ui_core::{
    math::{Point, Rect},
    paint::geometry::{GeometryPipeline, IndexType, Topology},
    test_support::app::TestApp,
};

use super::{
    axes::{ChartLayout, marker_bounds, value_y},
    data::PreparedData,
    paint::{CacheKey, Hover, line_geometry, measure, prepare, tooltip_lines},
    palette::LINE_WIDTH,
    test_support::{bounds, english, layout, prepared, prepared_all},
    text::ChartTextCache,
};
use crate::model::{CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow};

fn theme() -> Theme {
    Theme::of(Appearance::Light)
}

#[test]
fn a_window_of_days_should_become_one_bar_each() {
    let data = prepared_all(&english(), &[1_000.0, -500.0, 2_000.0]);
    let layout = layout(640.0, 360.0);
    let prepared = prepare(&data, layout);

    assert_eq!(prepared.bars.len(), 3);
    assert!(
        prepared
            .bars
            .iter()
            .all(|bar| layout.plot.contains(bar.bounds.origin)),
        "every bar is inside the plot"
    );
    assert_eq!(
        prepared
            .bars
            .iter()
            .map(|bar| bar.value)
            .collect::<Vec<_>>(),
        vec![1_000.0, -500.0, 2_000.0]
    );
    assert!(
        prepared
            .time_ticks
            .first()
            .is_some_and(|tick| tick.label.as_str() == "01-01"),
        "the time axis starts at the first day"
    );
}

#[test]
fn a_window_of_one_day_should_be_a_point_rather_than_a_line() {
    let data = prepared_all(&english(), &[1_000.0]);
    let prepared = prepare(&data, layout(640.0, 360.0));

    assert!(prepared.line.is_none(), "a line needs two points");
    let marker = prepared.marker.expect("one day is a reading to mark");
    assert!(prepared.bars.len() == 1);
    assert!(
        (marker.x - prepared.bars[0].bounds.origin.x - prepared.bars[0].bounds.size.width / 2.0)
            .abs()
            < 0.5,
        "the marker sits on the day the bar stands for"
    );
}

#[test]
fn a_day_that_is_not_a_reading_should_draw_no_bar_and_break_the_line() {
    let rows = [[1_000.0, 0.0], [f64::NAN, 0.0], [2_000.0, 0.0]];
    let translate = english();
    let data = PreparedData::new(
        &translate,
        super::test_support::dataset_rows(&rows),
        CryptocurrencyEtfFundSelection::All,
        CryptocurrencyEtfWindow::All,
    );
    let prepared = prepare(&data, layout(640.0, 360.0));

    assert_eq!(prepared.bars.len(), 2, "the middle day has no bar");
    let line = prepared.line.expect("the window has a line");
    assert_eq!(line.len(), 3);
    assert!(line[0].is_some(), "the first day is a reading");
    assert!(
        line[1].is_none(),
        "the day whose accumulation is not a number breaks the line"
    );
}

#[test]
fn a_named_fund_should_have_no_line_and_no_second_reading() {
    let fund = CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("AAA"));
    let data = prepared(
        &english(),
        &[1_000.0, 2_000.0],
        fund,
        CryptocurrencyEtfWindow::All,
    );
    let prepared = prepare(&data, layout(640.0, 360.0));

    assert!(prepared.line.is_none(), "one fund has no level to plot");
    assert!(prepared.marker.is_none());
    assert_eq!(prepared.bars.len(), 2, "but it still has a flow per day");
}

#[test]
fn a_wider_box_should_spread_the_same_days_further_apart() {
    let data = prepared_all(&english(), &[1_000.0, 2_000.0, 3_000.0]);
    let narrow = prepare(&data, layout(320.0, 240.0));
    let wide = prepare(&data, layout(1_280.0, 240.0));

    let spacing = |prepared: &super::paint::PreparedFrame| {
        prepared.bars[1].bounds.origin.x - prepared.bars[0].bounds.origin.x
    };
    assert!(
        spacing(&wide) > spacing(&narrow),
        "the same three days are further apart in a wider plot"
    );
    assert_eq!(
        wide.bars.len(),
        narrow.bars.len(),
        "a resize is not a different window"
    );
}

#[test]
fn a_cache_key_should_name_the_box_and_the_window_it_was_prepared_from() {
    let key = CacheKey {
        bounds: bounds(640.0, 360.0),
        revision: 3,
    };

    assert_eq!(
        key,
        CacheKey {
            bounds: bounds(640.0, 360.0),
            revision: 3
        }
    );
    assert_ne!(
        key,
        CacheKey {
            bounds: bounds(640.0, 361.0),
            revision: 3
        },
        "a resize prepares the frame again"
    );
    assert_ne!(
        key,
        CacheKey {
            bounds: bounds(640.0, 360.0),
            revision: 4
        },
        "so does another answer"
    );
}

#[test]
fn a_hover_should_be_the_same_hover_until_a_reading_or_a_whole_pixel_changes() {
    let hover = Hover {
        index: 2,
        cursor_y: 40.0,
    };

    assert_eq!(
        hover,
        Hover {
            index: 2,
            cursor_y: 40.0
        },
        "two positions on one day are one drawing"
    );
    assert_ne!(
        hover,
        Hover {
            index: 3,
            cursor_y: 40.0
        }
    );
    assert_ne!(
        hover,
        Hover {
            index: 2,
            cursor_y: 41.0
        }
    );
}

#[test]
fn a_hover_marker_should_be_centered_on_the_reading_it_marks() {
    let center = Point::new(120.0, 80.0);
    let marker = marker_bounds(center);

    assert_eq!(
        marker.origin.x + marker.size.width / 2.0,
        center.x,
        "the marker is centred on the reading, not hung from it"
    );
    assert_eq!(marker.origin.y + marker.size.height / 2.0, center.y);
    assert_eq!(marker.size.width, marker.size.height);
}

#[test]
fn a_tooltips_flow_should_colour_the_amount_and_not_the_caption() {
    let data = prepared_all(&english(), &[-500.0]);
    let point = &data.points[0];
    let theme = theme();
    let lines = tooltip_lines(point, true, &theme);
    let foreground = theme.components().tooltip.foreground;

    assert_eq!(lines.len(), 3, "the day, its flow, and what it holds");
    assert_eq!(
        lines[1][0].1, foreground,
        "the caption reads in the tooltip's own text colour"
    );
    assert_eq!(
        lines[1][1].1,
        theme.components().chart.bearish,
        "the amount keeps the colour the bar under it is drawn in"
    );
    assert_eq!(lines[1][0].0, "Daily net outflow (M): ");
    assert_eq!(lines[1][1].0, "-$500.00M");
}

#[test]
fn the_line_mesh_should_cover_every_segment_and_joint() {
    let points = [
        Some(Point::new(0.0, 0.0)),
        Some(Point::new(10.0, 10.0)),
        Some(Point::new(20.0, 0.0)),
    ];
    let desc = line_geometry(&points, LINE_WIDTH).expect("a line of three points has a mesh");
    eprintln!(
        "vertices={} indices={} bounds={:?}",
        desc.vertex_bytes.len(),
        desc.index_bytes.len(),
        desc.bounds
    );

    assert_eq!(desc.vertex_stride, GeometryPipeline::Ui.vertex_stride());
    assert_eq!(desc.index_type, IndexType::U32);
    assert_eq!(desc.topology, Topology::TriangleList);
    assert_eq!(desc.vertex_bytes.len(), 5 * 4 * 12);
    assert_eq!(desc.index_bytes.len(), 5 * 6 * 4);
    assert!(desc.bounds.size.width >= 20.0, "{:?}", desc.bounds);
    assert!(desc.bounds.size.height >= 10.0, "{:?}", desc.bounds);
}

#[test]
fn the_line_mesh_should_break_where_a_day_is_not_a_reading() {
    let points = [
        Some(Point::new(0.0, 0.0)),
        None,
        Some(Point::new(10.0, 10.0)),
    ];
    let desc = line_geometry(&points, LINE_WIDTH).expect("two runs of one point each");

    assert_eq!(desc.vertex_bytes.len(), 2 * 4 * 12);
}

#[test]
fn no_readings_should_have_no_line_mesh_at_all() {
    assert!(line_geometry(&[], LINE_WIDTH).is_none());
    assert!(line_geometry(&[None, None], LINE_WIDTH).is_none());
}

#[test]
fn the_axes_should_be_measured_wide_enough_for_the_titles_they_draw() {
    let data = prepared_all(&english(), &[1_000.0, 2_000.0]);
    let mut app = TestApp::new();
    let theme = theme();
    let mut text = ChartTextCache::default();

    let measurements = measure(app.app_mut(), &data, &mut text, &theme);
    let nothing_measured = super::paint::Measurements::default();

    assert!(
        measurements.left_axis_width > nothing_measured.left_axis_width,
        "an axis has to fit the labels it draws"
    );
    assert!(
        measurements
            .right_axis_width
            .is_some_and(|width| width > 0.0),
        "the whole complex draws a second axis"
    );
    assert_eq!(
        nothing_measured.right_axis_width, None,
        "a layout built before anything was measured falls back to its own minimum"
    );
}

#[test]
fn a_layout_should_place_the_marker_on_the_zero_line_of_its_own_axis() {
    let layout = ChartLayout::new(Rect::new(0.0, 0.0, 640.0, 360.0), 40.0, Some(60.0));
    let data = prepared_all(&english(), &[1_000.0, 2_000.0]);
    let scale = data.flow.domain;

    assert!(
        value_y(0.0, scale, layout.plot) <= layout.plot.origin.y + layout.plot.size.height,
        "a flow domain always contains zero, so the baseline is inside the plot"
    );
    assert!(layout.plot.size.height > 0.0);
}

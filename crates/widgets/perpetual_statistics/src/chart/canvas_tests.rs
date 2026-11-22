//! The canvas one chart draws: the box it fills, the pointer it follows, and the frames a
//! change is worth.
//!
//! These are the interface tests of the chart: they go through the engine's own
//! `WidgetTestApp`, because what they assert is what the engine ended up holding — a canvas
//! node, its primitives, and the frames a change is worth.

use std::sync::Arc;

use base_ui::{style_builder::StyleBuilder, theme::appearance::Appearance};
use i18n::language::codes::LanguageCode;
use market_participants_model::decimal::Quantity;
use smol_str::SmolStr;
use time::UtcOffset;
use ui_core::{
    math::Point,
    node::{builder::NodeBuilder, id::NodeId},
    paint::primitive::Primitive,
    window::WindowId,
};
use widget::test_support::WidgetTestApp;

use super::{canvas::Chart, open_interest::OpenInterestChart, paint::Hover, rect::RectExt};
use crate::{
    model::{OpenInterestPoint, StatisticsPoints, StatisticsSnapshot},
    service::StatisticsStreamMessage,
};

const WIDTH: f32 = 640.0;
const HEIGHT: f32 = 360.0;

/// The translator the product ships.
fn translate(language: LanguageCode) -> i18n::translate::Translate {
    i18n::translate::Translate::new(
        std::rc::Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

fn point(timestamp: i64, base: u64, quote: Option<u64>) -> OpenInterestPoint {
    OpenInterestPoint {
        timestamp,
        base_asset_volume: Quantity::from(base),
        quote_asset_volume: quote.map(Quantity::from),
    }
}

fn points(count: usize) -> Vec<OpenInterestPoint> {
    (0..count)
        .map(|index| {
            let step = u64::try_from(index).unwrap_or(0) + 1;
            point(
                i64::try_from(index).unwrap_or(0) * 300_000,
                step * 10,
                Some(step * 100),
            )
        })
        .collect()
}

fn snapshot(revision: u64, count: usize) -> StatisticsStreamMessage {
    StatisticsStreamMessage::Snapshot(Arc::new(StatisticsSnapshot {
        revision,
        metadata: None,
        points: StatisticsPoints::OpenInterest(points(count).into()),
    }))
}

struct ChartHarness {
    app: WidgetTestApp,
    chart: Chart<OpenInterestChart>,
    page: NodeId,
    window: WindowId,
}

impl ChartHarness {
    fn new(count: usize) -> Self {
        let mut app = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let window = app.window();
        let page = page(&mut app, WIDTH, HEIGHT);
        let mut chart = app.with_cx(|cx| {
            Chart::new(OpenInterestChart::new(
                cx.translate(),
                "BTC",
                "USDT",
                UtcOffset::UTC,
            ))
        });
        app.with_cx(|cx| chart.build(cx, page))
            .expect("the chart builds");
        // The first frame lays the page (and the canvas inside it) out.
        app.drain();
        app.with_cx(|_| chart.set_snapshot(&snapshot(1, count)));
        Self {
            app,
            chart,
            page,
            window,
        }
    }

    fn sync(&mut self) -> bool {
        let Self { app, chart, .. } = self;
        app.with_cx(|cx| chart.sync(cx)).expect("the chart syncs")
    }

    fn canvas(&self) -> &ui_core::paint::canvas::Canvas {
        self.chart
            .canvas_for_test(self.app.app(), self.window)
            .expect("the chart drew a canvas")
    }

    fn layout(&self) -> super::axes::ChartLayout {
        self.chart
            .layout_for_test()
            .expect("the chart was laid out")
    }

    fn resize(&mut self, width: f32, height: f32) {
        let page = self.page;
        self.app.with_cx(|cx| {
            let style = StyleBuilder::new()
                .width(width)
                .height(height)
                .flex_col()
                .build();
            cx.tree_mut()
                .set_style(page, style)
                .expect("the page resizes");
        });
        self.app.drain();
    }

    fn release(&mut self) {
        let Self { app, chart, .. } = self;
        app.with_cx(|cx| chart.release(cx));
    }
}

/// A box of a fixed size, which is the shape a chart is laid out in.
fn page(app: &mut WidgetTestApp, width: f32, height: f32) -> NodeId {
    app.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_PERPETUAL_STATISTICS_PAGE")
            .style(
                StyleBuilder::new()
                    .width(width)
                    .height(height)
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)
            .expect("the page builds")
    })
}

/// The middle of the plot, which is over one of the fixture's intervals.
fn middle_of_plot(harness: &ChartHarness) -> Point {
    let plot = harness.layout().plot;
    Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )
}

/// The atlas generation the chart's glyphs were rasterized in, if it draws any.
///
/// The leases a canvas holds are what an atlas reset invalidates, so reading the
/// generation back is how a test tells a canvas that was rebuilt from one that was kept.
fn glyph_generation(harness: &ChartHarness) -> Option<u32> {
    harness
        .canvas()
        .primitives()
        .iter()
        .find_map(|primitive| match primitive {
            Primitive::GlyphRun(run) => run.glyphs.first(),
            _ => None,
        })
        .map(|glyph| glyph.tile.generation)
}

/// Every primitive kind the canvas holds.
fn kinds(canvas: &ui_core::paint::canvas::Canvas) -> Vec<&'static str> {
    canvas
        .primitives()
        .iter()
        .map(|primitive| match primitive {
            Primitive::Quad(_) => "quad",
            Primitive::GlyphRun(_) => "text",
            Primitive::Path(_) => "path",
            Primitive::Image(_) => "image",
            Primitive::Shadow(_) => "shadow",
            Primitive::Clip(_) => "clip",
            Primitive::Canvas(_) => "canvas",
            Primitive::Geometry(_) => "geometry",
        })
        .collect()
}

#[test]
fn a_chart_should_fill_the_box_it_is_given() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync(), "the first frame paints");

    let layout = harness.layout();
    assert_eq!(layout.content.size.width, WIDTH);
    assert_eq!(layout.content.size.height, HEIGHT);
    assert_eq!(harness.canvas().bounds(), layout.content);
    let kinds = kinds(harness.canvas());
    assert!(kinds.contains(&"geometry"), "the two lines are meshes");
    assert!(kinds.contains(&"text"), "the axes are labelled");
    assert!(
        !harness.sync(),
        "a frame that changed nothing builds nothing (I3)"
    );
}

#[test]
fn a_steady_chart_should_not_request_a_frame() {
    let mut harness = ChartHarness::new(4);
    assert!(harness.sync());
    let requests = harness.app.frame_requests();

    assert!(!harness.sync(), "the second frame draws nothing");
    assert!(!harness.sync());
    assert_eq!(
        harness.app.frame_requests(),
        requests,
        "a steady chart asks for no frame (I3)"
    );
}

#[test]
fn a_resized_chart_should_redraw_itself_in_the_new_box() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    let before = harness.layout();

    harness.resize(WIDTH, 320.0);
    assert!(harness.sync(), "the new box is another drawing");
    let after = harness.layout();
    assert_eq!(after.content.size.height, 320.0);
    assert!(after.plot.size.height < before.plot.size.height);
    assert_eq!(harness.canvas().bounds(), after.content);
    assert!(!harness.sync(), "and it settles again");
}

#[test]
fn a_pane_narrower_than_the_chart_should_clip_it_rather_than_squeeze_the_plot() {
    let mut narrow = ChartHarness::new(3);
    narrow.resize(320.0, 240.0);
    assert!(narrow.sync());
    let mut wide = ChartHarness::new(3);
    assert!(wide.sync());

    assert_eq!(
        narrow.layout().content.size.width,
        super::axes::CHART_MIN_CONTENT_WIDTH,
        "the content keeps its own floor instead of taking the pane's width"
    );
    assert!(
        narrow.layout().plot.size.width >= super::axes::MINIMUM_PLOT_WIDTH,
        "the plot is never squeezed below its own minimum"
    );
    assert!(narrow.layout().plot.size.width < wide.layout().plot.size.width);
}

#[test]
fn a_pointer_over_the_plot_should_hover_the_interval_under_it() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());

    let position = middle_of_plot(&harness);
    assert!(
        harness.chart.point_at(position),
        "the pointer is over an interval"
    );
    let hover = harness
        .chart
        .hover_for_test()
        .expect("an interval is hovered");
    assert!(hover.index < 3);
    assert!(harness.sync(), "the hovered interval is drawn");
    let hovered = harness.canvas().primitives().len();
    assert!(
        !harness.chart.point_at(position),
        "the same interval is the same frame"
    );
    assert!(!harness.sync());
    assert_eq!(harness.canvas().primitives().len(), hovered);
}

#[test]
fn a_pointer_off_the_plot_should_not_hover_anything() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    let layout = harness.layout();

    assert!(!harness.chart.point_at(Point::new(1.0, 1.0)));
    assert_eq!(harness.chart.hover_for_test(), None);
    let below = Point::new(layout.plot.origin.x + 10.0, layout.time_axis.origin.y + 2.0);
    assert!(!harness.chart.point_at(below));
    assert_eq!(harness.chart.hover_for_test(), None);
}

#[test]
fn a_leaving_pointer_should_stop_hovering() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    let position = middle_of_plot(&harness);
    assert!(harness.chart.point_at(position));
    assert!(harness.sync());

    assert!(harness.chart.clear_hover(), "the pointer was on the plot");
    assert!(
        !harness.chart.clear_hover(),
        "a pointer that already left is not a change"
    );
    assert!(harness.sync(), "the frame without a hover is drawn");
    assert_eq!(
        harness.canvas().primitives().len(),
        harness.canvas().primitives().len()
    );
}

#[test]
fn a_snapshot_should_be_drawn_and_the_next_one_should_replace_it() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    let before = harness.canvas().primitives().len();

    harness
        .app
        .with_cx(|_| harness.chart.set_snapshot(&snapshot(2, 6)));
    assert!(harness.sync(), "a new window is a new drawing");
    assert_ne!(
        harness.canvas().primitives().len(),
        before,
        "more intervals draw more markers and labels"
    );
    assert_eq!(harness.chart.revision(), 2);
    assert!(!harness.sync());
}

#[test]
fn an_unchanged_window_should_not_redraw() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    let unchanged = snapshot(1, 3);
    let first = harness
        .app
        .with_cx(|_| harness.chart.set_snapshot(&unchanged));
    assert!(first, "a window this chart has not seen is a view");
    assert!(harness.sync(), "the new view is drawn");
    assert!(
        !harness
            .app
            .with_cx(|_| harness.chart.set_snapshot(&unchanged)),
        "the same window handed over again is not a new view"
    );
    assert!(!harness.sync(), "and it is not a new drawing either");
}

#[test]
fn a_language_switch_should_rewrite_the_labels() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());

    harness.app.switch_language(LanguageCode::ZhCn);
    let rebuilt = harness.app.with_cx(|cx| {
        let translate = cx.translate().clone();
        harness.chart.set_locale(&translate)
    });
    assert!(rebuilt, "another language is another set of labels");
    assert!(harness.sync(), "the rewritten labels are drawn");
    assert!(
        !harness.app.with_cx(|cx| {
            let translate = cx.translate().clone();
            harness.chart.set_locale(&translate)
        }),
        "the same language is not a change"
    );
}

/// The labels are drawn from shaped handles that lease glyph tiles from the atlas. When a
/// lease is gone — the atlas was reset, a font stack changed, a tile was evicted — the
/// engine refuses to encode those runs rather than draw reused texture, so a chart that
/// kept the canvas it already had would lose its labels for good. A moved lease is a frame
/// the chart owes.
#[test]
fn a_chart_should_reshape_the_labels_when_their_atlas_lease_is_gone() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync(), "the first frame paints");
    let labels = harness.canvas().primitives().len();
    let generation =
        glyph_generation(&harness).expect("a chart of three intervals labels its axes");

    // An atlas reset under the same font stack: every handle the canvas was built from is
    // leased from an atlas that no longer exists, while the glyphs shape back the same.
    let stack = harness.app.app().font_stack().clone();
    harness.app.app_mut().set_font_stack(stack);

    assert!(
        harness.sync(),
        "a label whose lease moved is a frame the chart owes, not an idle one"
    );
    assert_ne!(
        glyph_generation(&harness),
        Some(generation),
        "every label was shaped for the atlas that exists now"
    );
    assert_eq!(
        harness.canvas().primitives().len(),
        labels,
        "and none of them disappeared with the old lease"
    );
    assert!(
        !harness.sync(),
        "with fresh leases the frame is steady again (I3)"
    );
}

#[test]
fn a_zone_switch_should_rewrite_the_time_labels() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());

    assert!(
        harness
            .chart
            .set_time_zone(UtcOffset::from_hms(8, 0, 0).unwrap())
    );
    assert!(harness.sync(), "another zone is another label");
    assert!(
        !harness
            .chart
            .set_time_zone(UtcOffset::from_hms(8, 0, 0).unwrap()),
        "the same zone is the same labels"
    );
}

#[test]
fn another_appearance_should_repaint_the_canvas() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    let light = quad_fills(harness.canvas());

    let theme = harness.app.services().theme.clone();
    assert!(
        theme.set_appearance(harness.app.app_mut(), Appearance::Dark),
        "another appearance is a change the pane has to draw"
    );
    assert!(harness.sync(), "the dark frame is drawn");
    let dark = quad_fills(harness.canvas());
    assert_ne!(light, dark, "the canvas carries the new colours");
}

/// The fill of every quad a canvas holds, in drawing order.
fn quad_fills(canvas: &ui_core::paint::canvas::Canvas) -> Vec<ui_core::paint::color::Color> {
    canvas
        .primitives()
        .iter()
        .filter_map(|primitive| match primitive {
            Primitive::Quad(quad) => match quad.paint {
                ui_core::paint::primitive::QuadPaint::Solid { fill, .. } => Some(fill),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn another_window_should_hand_the_previous_meshes_back() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    assert_eq!(harness.app.app().geometry().len(), 2);

    harness
        .app
        .with_cx(|_| harness.chart.set_snapshot(&snapshot(2, 5)));
    assert!(harness.sync());

    assert_eq!(
        harness.app.app().geometry().len(),
        2,
        "the meshes of the window that was replaced are given back"
    );
}

#[test]
fn releasing_a_chart_should_hand_its_geometry_and_node_back() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    assert_eq!(
        harness.app.app().geometry().len(),
        2,
        "two lines are two registered meshes"
    );
    let nodes = harness.app.node_count();

    harness.release();
    assert_eq!(
        harness.app.app().geometry().len(),
        0,
        "the meshes are the chart's and go with it"
    );
    assert!(
        harness.app.node_count() < nodes,
        "the canvas node is removed"
    );
}

#[test]
fn a_window_of_one_interval_should_register_no_line() {
    let mut harness = ChartHarness::new(1);
    assert!(harness.sync());

    assert_eq!(
        harness.app.app().geometry().len(),
        0,
        "one point is a marker, not a line"
    );
    assert!(
        harness
            .chart
            .canvas_for_test(harness.app.app(), harness.window)
            .is_some()
    );
}

#[test]
fn an_empty_window_should_still_paint_its_axes() {
    let mut harness = ChartHarness::new(0);
    assert!(harness.sync());

    assert!(!harness.canvas().is_empty(), "the axes are still a frame");
    assert_eq!(harness.chart.point_count(), 0);
    assert_eq!(harness.app.app().geometry().len(), 0);
}

#[test]
fn a_hover_on_a_window_with_no_interval_should_not_hover() {
    let mut harness = ChartHarness::new(0);
    assert!(harness.sync());

    assert!(
        !harness.chart.point_at(middle_of_plot(&harness)),
        "there is no interval to hover"
    );
    assert_eq!(harness.chart.hover_for_test(), None);
}

#[test]
fn the_same_snapshot_should_not_move_the_hover() {
    let mut harness = ChartHarness::new(4);
    assert!(harness.sync());
    let position = middle_of_plot(&harness);
    assert!(harness.chart.point_at(position));
    let hover: Option<Hover> = harness.chart.hover_for_test();

    harness
        .app
        .with_cx(|_| harness.chart.set_snapshot(&snapshot(2, 4)));
    assert_eq!(
        harness.chart.hover_for_test(),
        hover,
        "a window of the same length keeps the interval under the pointer"
    );
}

#[test]
fn a_shrinking_window_should_drop_a_hover_it_no_longer_holds() {
    let mut harness = ChartHarness::new(4);
    assert!(harness.sync());
    let layout = harness.layout();
    let near_end = Point::new(
        layout.plot.right() - 14.0,
        layout.plot.origin.y + layout.plot.size.height / 2.0,
    );
    assert!(harness.chart.point_at(near_end));
    assert_eq!(
        harness.chart.hover_for_test().map(|hover| hover.index),
        Some(3)
    );

    harness
        .app
        .with_cx(|_| harness.chart.set_snapshot(&snapshot(2, 2)));
    assert_eq!(
        harness.chart.hover_for_test(),
        None,
        "the interval the pointer was on is gone"
    );
}

#[test]
fn the_chart_should_point_at_the_series_it_was_built_for() {
    let mut harness = ChartHarness::new(3);
    assert!(harness.sync());
    let layout = harness.layout();

    assert!(!layout.plot.is_empty());
    assert!(
        layout.left_axis.size.width > 0.0,
        "open interest has a left axis"
    );
    assert_eq!(
        SmolStr::new_static("open_interest").as_str(),
        "open_interest"
    );
}

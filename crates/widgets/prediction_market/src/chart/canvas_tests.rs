use std::{rc::Rc, sync::Arc};

use i18n::{embedded, language::codes::LanguageCode, translate::Translate};
use market_participants_model::{
    decimal::{Fraction, Ratio},
    prediction::{PredictionPricePoint, PredictionRange, PredictionSeries},
};
use ui_core::{
    math::{DevicePixelRatio, LogicalPoint, LogicalPx, LogicalRect, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::canvas::Canvas,
    test_support::record::PrimitiveKind,
    window::WindowId,
};
use ui_kit::theme::{appearance::Appearance, role::Role};
use widget::test_support::WidgetTestApp;

use super::{canvas::ChartCanvas, geometry::ChartLayout};
use crate::{
    model::PredictionMarketSnapshot,
    test_support::{charted_market, event_ref, event_with},
};

const WIDTH: LogicalPx = px(760.0);
const HEIGHT: LogicalPx = px(248.0);

fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(embedded::load().expect("the embedded locales should load")),
        language,
    )
}

fn snapshot() -> Arc<PredictionMarketSnapshot> {
    let event = event_ref();
    let (market, legs) = charted_market(&event, "charted");
    let view = event_with(vec![market.clone()]);
    let line = |end: f64| -> Vec<PredictionPricePoint> {
        (0..=60)
            .map(|minute| PredictionPricePoint {
                timestamp: minute * 60_000,
                probability: Fraction::new(Ratio::from_inner(
                    ((20.0 + (end - 20.0) * minute as f64 / 60.0) * 1e16).round() as u128,
                ))
                .expect("the fixture probability is in range"),
            })
            .collect()
    };
    let series = vec![
        PredictionSeries {
            leg: legs[0].clone(),
            points: line(26.0),
        },
        PredictionSeries {
            leg: legs[1].clone(),
            points: line(27.0),
        },
    ];
    Arc::new(PredictionMarketSnapshot::new(
        1,
        PredictionRange::All,
        view,
        market,
        series,
    ))
}

fn empty_snapshot() -> Arc<PredictionMarketSnapshot> {
    let event = event_ref();
    let (market, _) = charted_market(&event, "charted");
    let view = event_with(vec![market.clone()]);
    Arc::new(PredictionMarketSnapshot::new(
        1,
        PredictionRange::All,
        view,
        market,
        Vec::new(),
    ))
}

struct ChartHarness {
    app: WidgetTestApp,
    chart: ChartCanvas,
    page: NodeId,
    node: NodeId,
    window: WindowId,
    snapshot: Arc<PredictionMarketSnapshot>,
}

impl ChartHarness {
    fn new(snapshot: Arc<PredictionMarketSnapshot>) -> Self {
        let mut app = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let window = app.window();
        let page = page(&mut app, WIDTH, HEIGHT);
        let mut chart = app.with_cx(|_| ChartCanvas::new());
        let node = app
            .with_cx(|cx| chart.build(cx, page))
            .expect("the chart builds");
        app.drain();
        Self {
            app,
            chart,
            page,
            node,
            window,
            snapshot,
        }
    }

    fn sync(&mut self) -> bool {
        let Self {
            app,
            chart,
            snapshot,
            ..
        } = self;
        app.with_cx(|cx| chart.sync(cx, snapshot, None, None))
            .expect("the chart syncs")
    }

    fn sync_at(&mut self, pointer: LogicalPoint) -> bool {
        let Self {
            app,
            chart,
            snapshot,
            ..
        } = self;
        app.with_cx(|cx| chart.sync(cx, snapshot, Some(pointer), None))
            .expect("the chart syncs")
    }

    fn paint(&mut self) -> Vec<widget::test_support::CanvasPrimitive> {
        let _ = self.sync();
        self.app.drain();
        self.app.canvas_primitives(self.node)
    }

    fn canvas(&self) -> &Canvas {
        self.chart
            .canvas_for_test(self.app.app(), self.window)
            .expect("the chart drew a canvas")
    }

    fn box_of(&self) -> LogicalRect {
        self.app
            .canvas_handle(self.node)
            .expect("the node holds a canvas")
            .bounds()
    }

    fn resize(&mut self, width: LogicalPx, height: LogicalPx) {
        let page = self.page;
        self.app.with_cx(|cx| {
            let style = ui_core::paint::style_builder::StyleBuilder::new()
                .width(width)
                .height(height)
                .flex_col()
                .build();
            cx.tree_mut()
                .set_style(page, style)
                .expect("the page restyles");
        });
        self.app.drain();
    }

    fn change_appearance(&mut self, appearance: Appearance) {
        let theme = self.app.services().theme.clone();
        assert!(
            theme.set_appearance(self.app.app_mut(), appearance),
            "another appearance is a change the pane has to draw"
        );
    }

    fn release(&mut self) {
        let Self { app, chart, .. } = self;
        app.with_cx(|cx| chart.release(cx));
    }
}

fn page(app: &mut WidgetTestApp, width: LogicalPx, height: LogicalPx) -> NodeId {
    app.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_CANVAS_PAGE")
            .style(
                ui_core::paint::style_builder::StyleBuilder::new()
                    .width(width)
                    .height(height)
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)
            .expect("the page builds")
    })
}

fn kinds(primitives: &[widget::test_support::CanvasPrimitive]) -> Vec<PrimitiveKind> {
    primitives.iter().map(|primitive| primitive.kind).collect()
}

#[test]
fn a_chart_should_fill_the_box_it_is_given() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync(), "the first frame paints");

    assert_eq!(
        harness.box_of(),
        LogicalRect::new(px(0.0), px(0.0), WIDTH, HEIGHT),
        "the canvas covers the box the node was given"
    );
    assert!(
        !harness.canvas().is_empty(),
        "a chart of two lines draws a grid, axes, meshes and labels"
    );
    let kinds = kinds(&harness.paint());
    assert!(
        kinds.contains(&PrimitiveKind::Geometry),
        "the lines are meshes"
    );
    assert!(
        kinds.contains(&PrimitiveKind::GlyphRun),
        "the axes are labelled"
    );
    assert!(
        !harness.sync(),
        "a frame that changed nothing builds nothing (I3)"
    );
}

#[test]
fn a_steady_chart_should_not_request_a_frame() {
    let mut harness = ChartHarness::new(snapshot());
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
fn a_steady_canvas_should_be_kept() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    harness.app.drain();
    let drawn = harness
        .app
        .canvas_handle(harness.node)
        .expect("the node holds a canvas");

    assert!(!harness.sync(), "a steady frame paints nothing");
    assert!(!harness.sync(), "and keeps on painting nothing");
    let kept = harness
        .app
        .canvas_handle(harness.node)
        .expect("the node holds a canvas");
    assert!(
        Arc::ptr_eq(&drawn, &kept),
        "a steady frame keeps the canvas it is drawn from"
    );
}

#[test]
fn a_chart_should_not_draw_before_it_is_laid_out() {
    let mut app = WidgetTestApp::with_translate(translate(LanguageCode::En));
    let mut chart = app.with_cx(|_| ChartCanvas::new());
    let root = app.with_cx(|cx| cx.tree().root());
    app.with_cx(|cx| chart.build(cx, root))
        .expect("the chart builds");

    assert!(
        !app.with_cx(|cx| chart.sync(cx, &snapshot(), None, None))
            .expect("the chart syncs"),
        "a chart that was not laid out draws nothing"
    );
    assert_eq!(
        chart.prepared_builds(),
        0,
        "and it prepares nothing it cannot place"
    );
}

#[test]
fn another_appearance_should_repaint_the_chart_in_the_dark_roles() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    let light = harness
        .app
        .canvas_handle(harness.node)
        .expect("the node holds a canvas");
    let light_primitives = harness.paint();
    let light_theme = harness.app.services().theme.theme_for(Appearance::Light);
    assert!(
        light_primitives.iter().any(|primitive| {
            primitive.kind == PrimitiveKind::Quad
                && primitive.color == Some(light_theme.role(Role::ChartGrid))
        }),
        "the grid is drawn in the light theme's colour"
    );

    harness.change_appearance(Appearance::Dark);
    assert!(
        harness.sync(),
        "another appearance is a change the pane has to draw"
    );
    let dark = harness
        .app
        .canvas_handle(harness.node)
        .expect("the node holds a canvas");
    assert!(
        !Arc::ptr_eq(&light, &dark),
        "the redraw prepared a canvas of its own"
    );
    let band = harness.paint();
    let dark_theme = harness.app.services().theme.theme_for(Appearance::Dark);
    let dark_grid = band
        .iter()
        .find(|primitive| {
            primitive.kind == PrimitiveKind::Quad
                && primitive.color == Some(dark_theme.role(Role::ChartGrid))
        })
        .expect("the grid is drawn in the dark theme's colour");
    assert_eq!(
        dark_grid.color,
        Some(dark_theme.role(Role::ChartGrid)),
        "the dark appearance repaints the grid in the dark role"
    );
    let dark_line = band
        .iter()
        .find(|primitive| {
            primitive.kind == PrimitiveKind::Geometry
                && primitive.color == Some(dark_theme.role(Role::ChartSeries0))
        })
        .expect("a line is drawn in the dark theme's series colour");
    assert_eq!(
        dark_line.color,
        Some(dark_theme.role(Role::ChartSeries0)),
        "and every line in the dark series palette"
    );
}

#[test]
fn a_canvas_node_that_was_never_written_paints_no_primitives() {
    let harness = ChartHarness::new(snapshot());
    assert!(
        harness.app.canvas_primitives(harness.node).is_empty(),
        "no set_canvas means nothing inside the canvas"
    );
    assert_eq!(
        harness
            .app
            .paint_entries(harness.node)
            .iter()
            .filter(|entry| entry.kind == PrimitiveKind::Canvas)
            .count(),
        1,
        "the node is still one opaque canvas in the frame's paint"
    );
}

#[test]
fn releasing_a_chart_should_hand_its_geometry_and_node_back() {
    let mut harness = ChartHarness::new(snapshot());
    let baseline = harness.app.app().geometry().len();
    assert!(harness.sync());
    let primitives = harness.paint();
    let mesh = primitives
        .iter()
        .find(|primitive| primitive.kind == PrimitiveKind::Geometry)
        .expect("the canvas draws its meshes");
    assert!(
        mesh.geometry.is_some(),
        "the draw names the geometry it draws: {mesh:?}"
    );
    assert_eq!(
        harness.app.app().geometry().len(),
        baseline + 4,
        "two lines with a halo each are four registered meshes"
    );

    harness.release();
    assert_eq!(
        harness.app.app().geometry().len(),
        baseline,
        "releasing the chart returns its geometry"
    );
    assert!(
        harness.app.canvas_handle(harness.node).is_none(),
        "the released chart's canvas node is gone"
    );
}

#[test]
fn another_snapshot_should_repaint_and_hand_the_previous_meshes_back() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    harness.paint();
    let baseline = harness.app.app().geometry().len();
    let builds = harness.chart.prepared_builds();

    harness.snapshot = Arc::new((*harness.snapshot).clone());
    assert!(harness.sync(), "a new snapshot is a new drawing");
    assert_eq!(
        harness.chart.prepared_builds(),
        builds + 1,
        "the prepared frame is rebuilt for the new reading"
    );
    assert_eq!(
        harness.app.app().geometry().len(),
        baseline,
        "the meshes of the frame that was replaced are given back"
    );
    assert!(!harness.sync(), "and the frame settles again");
}

#[test]
fn a_pointer_should_resolve_to_a_sample_without_preparing_again() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    let builds = harness.chart.prepared_builds();
    let plot = ChartLayout::new(harness.box_of()).plot;
    let pointer = LogicalPoint::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    );

    let changed = harness
        .app
        .with_cx(|cx| {
            harness
                .chart
                .sync(cx, &harness.snapshot, Some(pointer), None)
        })
        .expect("the chart syncs");

    assert!(changed, "the hovered sample is another drawing");
    assert_eq!(
        harness.chart.prepared_builds(),
        builds,
        "a pointer that moved does not rebuild the prepared frame"
    );
    let hovered = harness
        .chart
        .hovered_sample()
        .expect("the pointer is over the plot");
    assert_eq!(
        hovered.timestamp, 1_800_000,
        "the middle of the window is the half-hour sample"
    );
    assert!(
        !harness
            .app
            .with_cx(|cx| harness
                .chart
                .sync(cx, &harness.snapshot, Some(pointer), None))
            .expect("the chart syncs"),
        "a pointer that did not move is a steady frame"
    );
}

#[test]
fn a_font_stack_change_should_not_rewrite_the_chart_canvas() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync(), "the first frame paints");
    harness.app.drain();
    let drawn = harness
        .app
        .canvas_handle(harness.node)
        .expect("the node holds a canvas");

    let window = harness.app.window();
    let stack = harness.app.app().font_stack().clone();
    harness.app.app_mut().set_font_stack(stack);
    harness
        .app
        .app_mut()
        .request_repaint(window)
        .expect("the host repaints after a font stack change");
    harness.app.drain();

    assert!(
        !harness.sync(),
        "the engine re-rasterizes canvas text; the chart owes no frame"
    );
    assert!(
        Arc::ptr_eq(
            &drawn,
            &harness
                .app
                .canvas_handle(harness.node)
                .expect("the node holds a canvas")
        ),
        "the canvas value must survive a font stack change"
    );
    assert!(
        kinds(&harness.app.canvas_primitives(harness.node)).contains(&PrimitiveKind::GlyphRun),
        "the rendered frame still draws the labels"
    );
}

#[test]
fn a_dpi_change_should_not_rewrite_a_hovered_chart_canvas() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync(), "the first frame paints");
    let plot = ChartLayout::new(harness.box_of()).plot;
    let pointer = LogicalPoint::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    );
    assert!(harness.sync_at(pointer), "the hovered frame paints");
    harness.app.drain();
    let drawn = harness
        .app
        .canvas_handle(harness.node)
        .expect("the node holds a canvas");

    let window = harness.app.window();
    harness
        .app
        .app_mut()
        .window_mut(window)
        .expect("the host's window exists")
        .set_device_pixel_ratio(DevicePixelRatio::new(2.0).expect("2x is a valid ratio"));
    harness.app.drain();

    assert!(
        !harness.sync_at(pointer),
        "the engine rasterizes canvas text per window DPR; the chart owes no frame"
    );
    assert!(
        Arc::ptr_eq(
            &drawn,
            &harness
                .app
                .canvas_handle(harness.node)
                .expect("the node holds a canvas")
        ),
        "the canvas value must survive a DPI change"
    );
    assert!(
        kinds(&harness.app.canvas_primitives(harness.node)).contains(&PrimitiveKind::GlyphRun),
        "the rendered frame still draws the labels at the new DPR"
    );
}

#[test]
fn clearing_the_chart_should_draw_the_next_frame_from_scratch() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    harness.paint();
    let builds = harness.chart.prepared_builds();
    assert!(!harness.sync(), "the drawn frame is steady");

    harness.chart.clear_for_test();
    assert!(harness.sync(), "a cleared chart owes a frame");
    assert_eq!(
        harness.chart.prepared_builds(),
        builds + 1,
        "the cleared frame is prepared again"
    );
    assert_eq!(
        harness.app.app().geometry().len(),
        4,
        "and its meshes were replaced, not leaked"
    );
}

#[test]
fn a_language_switch_should_rewrite_the_labels() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    harness.paint();
    let builds = harness.chart.prepared_builds();

    harness.app.switch_language(LanguageCode::ZhCn);
    assert!(harness.sync(), "another language is another set of labels");
    assert_eq!(
        harness.chart.prepared_builds(),
        builds + 1,
        "the axis labels are written in the language showing"
    );
    assert!(
        kinds(&harness.paint()).contains(&PrimitiveKind::GlyphRun),
        "the rewritten labels are drawn"
    );
    assert!(!harness.sync(), "and the frame settles");
}

#[test]
fn an_empty_snapshot_should_draw_no_series_meshes() {
    let mut harness = ChartHarness::new(empty_snapshot());
    let baseline = harness.app.app().geometry().len();
    assert!(
        harness.sync(),
        "the empty frame is still a canvas of its own"
    );

    assert_eq!(
        harness.app.app().geometry().len(),
        baseline,
        "a snapshot with no samples draws no lines and registers no meshes"
    );
    assert!(
        harness
            .app
            .paint_entries(harness.node)
            .iter()
            .any(|entry| entry.kind == PrimitiveKind::Canvas),
        "the node is still painted"
    );
    assert!(!harness.sync(), "an empty reading is a steady frame too");
}

#[test]
fn a_resize_should_prepare_the_new_box() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    harness.paint();
    let builds = harness.chart.prepared_builds();
    let before = ChartLayout::new(harness.box_of()).plot;

    harness.resize(WIDTH, HEIGHT + px(100.0));
    assert!(harness.sync(), "the new box is another drawing");
    let after = ChartLayout::new(harness.box_of()).plot;

    assert_eq!(
        harness.chart.prepared_builds(),
        builds + 1,
        "a box of another size is a prepared frame of another size"
    );
    assert!(after.size.height > before.size.height, "the plot grew");
    assert!(!harness.sync(), "and it settles again");
}

#[test]
fn a_fractional_resize_should_not_prepare_again() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    harness.paint();
    let builds = harness.chart.prepared_builds();
    let box_before = harness.box_of();

    harness.resize(WIDTH + px(0.3), HEIGHT + px(0.3));

    assert_eq!(
        harness.box_of(),
        box_before,
        "the rounded box is the same box"
    );
    assert!(
        !harness.sync(),
        "a fractional resize is not a drawing (the layout is rounded to whole pixels)"
    );
    assert_eq!(
        harness.chart.prepared_builds(),
        builds,
        "and it prepared nothing"
    );
}

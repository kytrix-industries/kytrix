//! The canvas one prediction chart draws: the box it fills, the pointer it follows,
//! and the frames a change is worth.
//!
//! These are the interface tests of the chart: they go through the engine's own
//! `WidgetTestApp`, because what they assert is what the engine ended up holding — a
//! canvas node, its primitives, its registered geometry, and the frames a change is
//! worth. The four rules `crates/widgets/AGENTS.md` §11 freezes are here:
//!
//! - **A** a steady `sync` writes no canvas and asks for no frame (the same `Arc` is
//!   still in the node);
//! - **B** an appearance switch repaints in the appearance's own roles;
//! - **C** a canvas node that was never written paints no primitives, though the node
//!   is still one opaque canvas — a node that stopped being a canvas is what this
//!   turns red. Deleting the chart's `set_canvas` is caught by the tests that assert
//!   the primitives it wrote (A, B and D), not by C;
//! - **D** releasing the chart hands its registered geometry back.

use std::{rc::Rc, sync::Arc};

use base_ui::theme::{appearance::Appearance, role::Role};
use i18n::{global::embedded, language::codes::LanguageCode, translate::Translate};
use market_participants_model::{
    decimal::{Fraction, Ratio},
    prediction::{PredictionPricePoint, PredictionRange, PredictionSeries},
};
use ui_core::{
    math::{Point, Rect},
    node::{builder::NodeBuilder, id::NodeId},
    paint::canvas::Canvas,
    test_support::record::PrimitiveKind,
    window::WindowId,
};
use widget::test_support::WidgetTestApp;

use super::{canvas::ChartCanvas, geometry::ChartLayout};
use crate::{
    model::PredictionMarketSnapshot,
    test_support::{charted_market, event_ref, event_with},
};

/// The chart's page. It has to fit the headless host's 800 x 600 window: the engine
/// clamps a child to the window it is laid out in.
const WIDTH: f32 = 760.0;
const HEIGHT: f32 = 248.0;

/// The catalogue the chart formats through.
fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(embedded::load().expect("the embedded locales should load")),
        language,
    )
}

/// Two lines, one sample a minute over an hour, running from 20 % to the given ends.
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

/// A snapshot with no samples at all.
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

/// One chart mounted under a page of a fixed size.
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
        // The first frame lays the page (and the canvas inside it) out; the chart's own
        // `sync` is what fills the node in, and every test here calls it itself.
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

    /// Syncs one frame with the pointer over `pointer`.
    fn sync_at(&mut self, pointer: Point) -> bool {
        let Self {
            app,
            chart,
            snapshot,
            ..
        } = self;
        app.with_cx(|cx| chart.sync(cx, snapshot, Some(pointer), None))
            .expect("the chart syncs")
    }

    /// Paints one frame and returns the primitives it drew.
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

    fn box_of(&self) -> Rect {
        self.app
            .canvas_handle(self.node)
            .expect("the node holds a canvas")
            .bounds()
    }

    fn resize(&mut self, width: f32, height: f32) {
        let page = self.page;
        self.app.with_cx(|cx| {
            let style = base_ui::style_builder::StyleBuilder::new()
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

/// A box of a fixed size, which is the shape a chart is laid out in.
fn page(app: &mut WidgetTestApp, width: f32, height: f32) -> NodeId {
    app.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_CANVAS_PAGE")
            .style(
                base_ui::style_builder::StyleBuilder::new()
                    .width(width)
                    .height(height)
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)
            .expect("the page builds")
    })
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
            ui_core::paint::primitive::Primitive::GlyphRun(run) => run.glyphs.first(),
            _ => None,
        })
        .map(|glyph| glyph.tile.generation)
}

/// The kinds of a canvas' primitives, in paint order.
fn kinds(primitives: &[widget::test_support::CanvasPrimitive]) -> Vec<PrimitiveKind> {
    primitives.iter().map(|primitive| primitive.kind).collect()
}

#[test]
fn a_chart_should_fill_the_box_it_is_given() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync(), "the first frame paints");

    assert_eq!(
        harness.box_of(),
        Rect::new(0.0, 0.0, WIDTH, HEIGHT),
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

/// **Rule A.** A steady `sync` must not replace the canvas the node holds: the same
/// `Arc` across two syncs is what proves `set_canvas` was not called.
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

    // No frame has laid the node out yet: it has no box, so there is nothing to draw.
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

/// **Rule B.** A canvas bakes its colours in, so an appearance switch is a repaint in
/// the appearance's own roles — not the light ones it was built with.
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

/// **Rule C.** A canvas node that was never written paints no primitives; the node
/// itself is still one opaque canvas in the frame's paint. What turns this red is a
/// build that stops making the node a canvas — the frame the chart's `set_canvas`
/// feeds is guarded by the tests that assert the primitives it wrote (A, B and D).
#[test]
fn a_canvas_node_that_was_never_written_paints_no_primitives() {
    let harness = ChartHarness::new(snapshot());
    // No `sync` was called: the node holds the empty canvas it was built with, and the
    // frame the harness drained is still the one this reads.
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

/// **Rule D.** The series lines are registered meshes; releasing the chart hands them
/// back to the process-wide store, and takes its canvas node with it.
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

/// A pointer that moved is another sample, not another frame of geometry: the prepared
/// chart is reused and only the canvas is rebuilt.
#[test]
fn a_pointer_should_resolve_to_a_sample_without_preparing_again() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    let builds = harness.chart.prepared_builds();
    let plot = ChartLayout::new(harness.box_of()).plot;
    let pointer = Point::new(
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

/// The labels are drawn from shaped handles that lease glyph tiles from the atlas. When
/// a lease is gone — the atlas was reset or a font stack changed — the engine refuses to
/// encode those runs rather than draw reused texture, so a chart that kept the canvas
/// it already had would lose its labels for good. A moved lease is a frame the chart
/// owes.
#[test]
fn a_stale_atlas_lease_should_force_a_repaint() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync(), "the first frame paints");
    let labels = harness.canvas().primitives().len();
    let generation = glyph_generation(&harness).expect("the axes are labelled");

    // An atlas reset under the same font stack: every handle the canvas was built from
    // is leased from an atlas that no longer exists, while the glyphs shape back the
    // same.
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

/// The stale-lease guard looks at the handles the canvas on screen was built from, not
/// at every handle the shaping cache still holds. A label an earlier pointer sample
/// left behind is in the cache but not on the canvas, and a chart that redrew for it
/// would never settle again (I3).
#[test]
fn a_stale_label_off_the_canvas_should_not_keep_the_chart_repainting() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync(), "the first frame paints");
    let plot = ChartLayout::new(harness.box_of()).plot;
    let sample = |fraction: f32| {
        Point::new(
            plot.origin.x + plot.size.width * fraction,
            plot.origin.y + plot.size.height / 2.0,
        )
    };
    let first = sample(0.25);
    let second = sample(0.75);
    assert!(
        harness.sync_at(first),
        "the first hovered sample is a drawing"
    );
    assert!(harness.sync_at(second), "another sample is another drawing");

    // An atlas reset invalidates every lease, including the labels of the sample the
    // pointer has left: those stay in the shaping cache but are not on the canvas.
    let stack = harness.app.app().font_stack().clone();
    harness.app.app_mut().set_font_stack(stack);
    assert!(
        harness.sync_at(second),
        "the labels on screen are shaped for the atlas that exists now"
    );
    assert!(
        !harness.sync_at(second),
        "a stale label the canvas does not draw must not keep the chart repainting (I3)"
    );
}

/// Clearing the chart is what the pane does when it can no longer vouch for a reading:
/// the prepared frame is dropped, and the frame after it draws from scratch.
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

    harness.resize(WIDTH, HEIGHT + 100.0);
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

/// The box the chart lays itself out in is rounded to whole pixels, so a resize that
/// only moves the fraction of a pixel is not a new frame.
#[test]
fn a_fractional_resize_should_not_prepare_again() {
    let mut harness = ChartHarness::new(snapshot());
    assert!(harness.sync());
    harness.paint();
    let builds = harness.chart.prepared_builds();
    let box_before = harness.box_of();

    harness.resize(WIDTH + 0.3, HEIGHT + 0.3);

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

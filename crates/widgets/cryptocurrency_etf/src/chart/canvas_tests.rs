//! The canvas one chart draws: the box it fills, the pointer it follows, and the labels
//! it rewrites when the language moves under it.
//!
//! These are the interface tests of the chart: they go through the engine's own
//! `WidgetTestApp` (the headless host a widget is driven by), because what they assert is
//! what the engine ended up holding — a canvas node, its primitives, and the frames a
//! change is worth.

use base_ui::style_builder::StyleBuilder;
use i18n::language::codes::LanguageCode;
use ui_core::{
    math::Point,
    node::{builder::NodeBuilder, id::NodeId},
    paint::primitive::Primitive,
    window::WindowId,
};
use widget::test_support::WidgetTestApp;

use super::{
    axes::nearest_index,
    canvas::CryptocurrencyEtfChart,
    paint::Hover,
    test_support::{dataset, translate},
};
use crate::model::{CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow};

const WIDTH: f32 = 640.0;
const HEIGHT: f32 = 360.0;

/// One chart, drawn into a page of a fixed size.
struct ChartHarness {
    app: WidgetTestApp,
    chart: CryptocurrencyEtfChart,
    window: WindowId,
}

impl ChartHarness {
    fn sync(&mut self) -> bool {
        let Self { app, chart, .. } = self;
        app.with_cx(|cx| chart.sync(cx)).expect("the chart syncs")
    }

    fn sync_locale(&mut self) -> bool {
        let Self { app, chart, .. } = self;
        app.with_cx(|cx| chart.sync_locale(cx))
    }

    /// Points the chart at another history, the way a new answer does.
    fn set_view(&mut self) -> bool {
        let Self { app, chart, .. } = self;
        app.with_cx(|cx| {
            chart.set_view(
                cx,
                dataset(&[500.0, -250.0, 1_500.0, 3_000.0]),
                CryptocurrencyEtfFundSelection::All,
                CryptocurrencyEtfWindow::All,
            )
        })
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

    fn release(&mut self) {
        let Self { app, chart, .. } = self;
        app.with_cx(|cx| chart.release(cx));
    }
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

/// Builds a chart over a page of `width` × `height`, and lays it out.
fn harness_in(width: f32, height: f32, flows: &[f64]) -> ChartHarness {
    let mut app = WidgetTestApp::with_translate(translate(LanguageCode::En));
    let window = app.window();
    let page = page(&mut app, width, height);
    let mut chart = app.with_cx(|cx| {
        CryptocurrencyEtfChart::new(
            cx,
            dataset(flows),
            CryptocurrencyEtfFundSelection::All,
            CryptocurrencyEtfWindow::All,
        )
    });
    app.with_cx(|cx| chart.build(cx, page))
        .expect("the chart builds");
    // The first frame lays the page (and the canvas inside it) out.
    app.drain();
    ChartHarness { app, chart, window }
}

fn harness() -> ChartHarness {
    harness_in(WIDTH, HEIGHT, &[1_000.0, -500.0, 2_000.0])
}

/// A box of a fixed size, which is the shapes a chart is laid out in.
fn page(app: &mut WidgetTestApp, width: f32, height: f32) -> NodeId {
    app.with_cx(|cx| {
        let root = cx.tree().root();
        NodeBuilder::boxed()
            .id("TEST_ETF_PAGE")
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

/// The middle of the plot, which is over one of the fixture's days.
fn middle_of_plot(harness: &ChartHarness) -> Point {
    let plot = harness.layout().plot;
    Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )
}

#[test]
fn a_chart_should_fill_the_box_it_is_given() {
    let mut harness = harness();
    assert!(harness.sync(), "the first frame paints");

    let layout = harness.layout();
    assert_eq!(
        layout.content.size.width, WIDTH,
        "the content box is the pane's, when the labels fit it"
    );
    assert_eq!(layout.content.size.height, HEIGHT);
    assert_eq!(
        harness.canvas().bounds(),
        layout.content,
        "the canvas covers what the frame drew"
    );
    assert!(
        !harness.canvas().is_empty(),
        "a chart of three days draws axes, a grid and three bars"
    );
    assert!(
        !harness.sync(),
        "a frame that changed nothing builds nothing (I3)"
    );
}

/// The content box is the pane's when the pane is roomier than the floor the layout keeps
/// for its labels: that floor is a minimum, not a size the chart insists on.
#[test]
fn a_chart_in_a_roomy_box_should_fill_it_rather_than_stay_at_its_floor() {
    // Taller and wider than the 640×360 floor, inside the headless host's 800×600 window.
    let mut harness = harness_in(760.0, 560.0, &[1_000.0, -500.0, 2_000.0]);
    assert!(harness.sync());

    let layout = harness.layout();
    assert_eq!(
        layout.content.size.width, 760.0,
        "the content follows the box it is given"
    );
    assert_eq!(layout.content.size.height, 560.0);
    assert_eq!(
        harness.canvas().bounds(),
        layout.content,
        "the canvas covers what the frame drew"
    );
}

#[test]
fn a_pane_narrower_than_the_chart_should_clip_it_rather_than_squeeze_the_plot() {
    let mut small = harness_in(320.0, 240.0, &[1_000.0, 2_000.0]);
    assert!(small.sync());
    let mut wide = harness_in(WIDTH, HEIGHT, &[1_000.0, 2_000.0]);
    assert!(wide.sync());

    assert_eq!(
        small.layout().content.size.width,
        WIDTH,
        "the content keeps its own floor, so the labels are never crushed"
    );
    assert_eq!(
        small.layout().content.size.height,
        HEIGHT,
        "and so does its height"
    );
    assert_eq!(
        small.layout().plot.size.width,
        wide.layout().plot.size.width,
        "a pane narrower than the chart's floor clips it rather than squeezing the plot"
    );
}

#[test]
fn a_pointer_over_the_plot_should_hover_the_day_under_it() {
    let mut harness = harness();
    assert!(harness.sync());

    let position = middle_of_plot(&harness);
    assert!(
        harness.chart.point_at(position),
        "the pointer is over a day"
    );
    let hover = harness.chart.hover_for_test().expect("the day is hovered");
    assert_eq!(
        Some(hover.index),
        nearest_index(harness.layout().plot, 3, position.x),
        "the reading the pointer is nearest to"
    );
    assert!(
        !harness.chart.point_at(position),
        "the same day is the same frame"
    );
    assert!(harness.sync(), "the hovered day is drawn");
}

#[test]
fn a_pointer_off_the_plot_should_not_hover_anything() {
    let mut harness = harness();
    assert!(harness.sync());
    let layout = harness.layout();

    assert!(!harness.chart.point_at(Point::new(1.0, 1.0)));
    assert_eq!(harness.chart.hover_for_test(), None);
    let below = Point::new(layout.plot.origin.x + 10.0, layout.time_axis.origin.y + 2.0);
    assert!(
        !harness.chart.point_at(below),
        "the time axis is not the plot"
    );
    assert_eq!(harness.chart.hover_for_test(), None);
}

#[test]
fn a_leaving_pointer_should_stop_hovering() {
    let mut harness = harness();
    assert!(harness.sync());
    assert!(harness.chart.point_at(middle_of_plot(&harness)));
    assert!(harness.chart.hover_for_test().is_some());

    assert!(harness.chart.clear_hover(), "the pointer left the plot");
    assert!(!harness.chart.clear_hover(), "and leaving twice is no news");
    assert_eq!(harness.chart.hover_for_test(), None);
}

#[test]
fn a_hover_should_draw_a_crosshair_and_a_tooltip_on_the_canvas() {
    let mut harness = harness();
    assert!(harness.sync());
    let quiet = harness.canvas().primitives().len();

    assert!(harness.chart.point_at(middle_of_plot(&harness)));
    assert!(harness.sync(), "the hovered frame paints");
    let hovered = harness.canvas().primitives().len();
    assert!(
        hovered > quiet,
        "a hover adds a crosshair, a marker and a tooltip ({quiet} → {hovered})"
    );
}

#[test]
fn a_pointers_height_should_be_kept_at_whole_pixels() {
    let mut harness = harness();
    assert!(harness.sync());
    let plot = harness.layout().plot;
    let y = plot.origin.y + 10.4;

    assert!(
        harness
            .chart
            .point_at(Point::new(plot.origin.x + plot.size.width / 2.0, y))
    );
    let hover = harness.chart.hover_for_test().expect("the day is hovered");
    assert_eq!(
        hover,
        Hover {
            index: hover.index,
            cursor_y: y.round(),
        },
        "a pointer that moved less than a pixel is not a change worth a frame"
    );
}

#[test]
fn a_language_change_should_rewrite_the_labels_a_chart_is_already_showing() {
    let mut harness = harness();
    assert!(harness.sync());
    let english_primitives = harness.canvas().primitives().len();

    harness.app.switch_language(LanguageCode::ZhCn);
    assert!(
        harness.sync_locale(),
        "another language is another set of labels"
    );
    assert!(
        !harness.sync_locale(),
        "and it is resolved once, not every frame"
    );
    assert!(harness.sync(), "the new labels are drawn");
    assert!(
        !harness.canvas().is_empty(),
        "the same chart says the same things in another language ({english_primitives} primitives before)"
    );
}

/// The labels are drawn from shaped handles that lease glyph tiles from the atlas. When a
/// lease is gone — the atlas was reset, a font stack changed, a tile was evicted — the
/// engine refuses to encode those runs rather than draw reused texture, so a chart that
/// kept the canvas it already had would lose its labels for good. A moved lease is a
/// frame the chart owes.
#[test]
fn a_chart_should_reshape_the_labels_when_their_atlas_lease_is_gone() {
    let mut harness = harness();
    assert!(harness.sync(), "the first frame paints");
    let labels = harness.canvas().primitives().len();
    let generation = glyph_generation(&harness).expect("a chart of three days labels its axes");

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
fn a_chart_should_keep_the_readings_it_was_given() {
    let harness = harness();
    let metrics = harness
        .chart
        .metrics()
        .expect("a window with days has a reading");

    assert_eq!(metrics.date.as_str(), "2024-01-03");
    assert_eq!(metrics.flow.as_str(), "+$2.00B");
    assert_eq!(metrics.net_assets.as_deref(), Some("$2.50B"));
}

#[test]
fn releasing_a_chart_should_leave_no_geometry_behind() {
    let mut harness = harness();
    assert!(harness.sync());
    assert_eq!(
        harness.app.app().geometry().len(),
        1,
        "the net-asset line is a registered mesh"
    );

    harness.release();
    assert_eq!(
        harness.app.app().geometry().len(),
        0,
        "the mesh is the chart's own, so it goes with it"
    );
}

/// A new answer is a new prepared frame, and another net-asset line with it: the mesh the
/// frame before it registered is handed back rather than stacked behind the new one.
#[test]
fn preparing_another_frame_should_hand_the_previous_geometry_back() {
    let mut harness = harness();
    assert!(harness.sync());
    assert_eq!(
        harness.app.app().geometry().len(),
        1,
        "the net-asset line is one registered mesh"
    );

    assert!(harness.set_view(), "another answer is a new view");
    assert!(harness.sync(), "and a new frame");
    assert_eq!(
        harness.app.app().geometry().len(),
        1,
        "the mesh of the frame before it was released, not kept beside it"
    );
}

#[test]
fn a_chart_should_not_draw_before_it_is_laid_out() {
    let mut app = WidgetTestApp::with_translate(translate(LanguageCode::En));
    let page = page(&mut app, WIDTH, HEIGHT);
    let mut chart = app.with_cx(|cx| {
        CryptocurrencyEtfChart::new(
            cx,
            dataset(&[1_000.0]),
            CryptocurrencyEtfFundSelection::All,
            CryptocurrencyEtfWindow::All,
        )
    });
    app.with_cx(|cx| chart.build(cx, page))
        .expect("the chart builds");

    // No frame has laid the page out yet: the canvas node has no box, so there is
    // nothing to draw into and nothing to draw.
    assert!(
        !app.with_cx(|cx| chart.sync(cx)).expect("the chart syncs"),
        "a chart that was not laid out draws nothing"
    );
}

#[test]
fn the_plot_should_be_a_rectangle_inside_the_content() {
    let mut harness = harness();
    assert!(harness.sync());
    let layout = harness.layout();

    assert!(layout.plot.size.width > 0.0 && layout.plot.size.height > 0.0);
    assert!(
        layout.plot.origin.x >= layout.content.origin.x
            && layout.plot.origin.x + layout.plot.size.width
                <= layout.content.origin.x + layout.content.size.width,
        "the plot is inside the content box"
    );
    assert_eq!(
        layout.left_axis.origin.y, layout.plot.origin.y,
        "both axes share the plot's rows"
    );
    assert_eq!(
        layout.right_axis.origin.x,
        layout.plot.origin.x + layout.plot.size.width,
        "the right axis starts where the plot ends"
    );
}

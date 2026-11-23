//! What a frame of the prediction chart actually draws, and what it reuses.
//!
//! A chart is a canvas, so what it draws is read back as the canvas' primitives: the
//! quads (grid dots, the crosshair, label boxes and their bars, dots and pulse rings),
//! the clip brackets and the registered meshes every series line draws. What a run of
//! text *says* is not in the scene once shaped, so the paint pass reports the lines it
//! drew into a small test-only recorder (`super::paint::take_text_log`). What is pinned
//! here is the hover's whole shape — the crosshair's place, the fade split, the label
//! column's side and stack, the time label — plus the cache rule that a pointer-only
//! frame prepares nothing.
//!
//! There is no pixel capture: on Windows the test window has no headless renderer, so
//! a PNG is not possible. These are geometry and paint-call assertions.

use std::{rc::Rc, sync::Arc};

use base_ui::theme::{appearance::Appearance, resolved::Theme, role::Role};
use i18n::{embedded, language::codes::LanguageCode, translate::Translate};
use market_participants_model::{
    decimal::{Fraction, Ratio},
    prediction::{PredictionPricePoint, PredictionRange, PredictionSeries},
};
use ui_core::{
    math::{Point, Rect},
    paint::color::Color,
    test_support::record::PrimitiveKind,
};
use widget::test_support::{CanvasPrimitive, WidgetTestApp};

use super::{
    canvas::ChartCanvas,
    geometry::{ChartLayout, RectExt, ValueAxis, y_for_percent},
    hover::LABEL_HEIGHT,
    paint::{TextRecord, reset_text_log, take_text_log},
};
use crate::{
    model::PredictionMarketSnapshot,
    test_support::{charted_market, event_ref, event_with},
};

/// The chart's page. It has to fit the headless host's 800 x 600 window: the engine
/// clamps a child to the window it is laid out in.
const WINDOW_WIDTH: f32 = 760.0;
const WINDOW_HEIGHT: f32 = 248.0;

/// One sample per minute over an hour.
const SAMPLE_COUNT: i64 = 60;
const MINUTE: i64 = 60_000;
/// The step the label boxes keep from each other.
const LABEL_GAP: f32 = 1.0;

/// The catalogue the chart formats through.
fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(embedded::load().expect("the embedded locales should load")),
        language,
    )
}

/// One chart mounted under a page of a fixed size.
struct ChartHarness {
    app: WidgetTestApp,
    chart: ChartCanvas,
    page: ui_core::node::id::NodeId,
    snapshot: Arc<PredictionMarketSnapshot>,
    pointer: Option<Point>,
    pulse: Option<f32>,
}

impl ChartHarness {
    fn new(snapshot: Arc<PredictionMarketSnapshot>) -> Self {
        Self::sized(snapshot, WINDOW_WIDTH, WINDOW_HEIGHT)
    }

    fn sized(snapshot: Arc<PredictionMarketSnapshot>, width: f32, height: f32) -> Self {
        let mut app = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let page = page(&mut app, width, height);
        let chart = app.with_cx(|_| ChartCanvas::new());
        let mut harness = Self {
            app,
            chart,
            page,
            snapshot,
            pointer: None,
            pulse: None,
        };
        let page = harness.page;
        let node = harness
            .app
            .with_cx(|cx| harness.chart.build(cx, page))
            .expect("the chart builds");
        assert!(harness.app.has_canvas(node), "the body built a canvas node");
        // The first frame lays the page (and the canvas inside it) out.
        harness.app.drain();
        harness
    }

    fn sync(&mut self) -> bool {
        let Self {
            app,
            chart,
            snapshot,
            pointer,
            pulse,
            ..
        } = self;
        app.with_cx(|cx| chart.sync(cx, snapshot, *pointer, *pulse))
            .expect("the chart syncs")
    }

    fn node(&self) -> ui_core::node::id::NodeId {
        self.chart.node().expect("the chart built a canvas node")
    }

    /// Draws the frame the current pointer and pulse ask for, and reads it back.
    fn draw(&mut self) -> Frame {
        reset_text_log();
        let _ = self.sync();
        self.app.drain();
        let node = self.node();
        Frame {
            primitives: self.app.canvas_primitives(node),
            texts: take_text_log(),
            bounds: self
                .app
                .canvas_handle(node)
                .expect("the chart drew a canvas")
                .bounds(),
        }
    }

    fn set_pointer(&mut self, pointer: Option<Point>) {
        self.pointer = pointer;
    }

    fn set_pulse(&mut self, pulse: Option<f32>) {
        self.pulse = pulse;
    }

    /// Lays the page out at another size, the way a resize does.
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
}

/// A box of a fixed size, which is the shape a chart is laid out in.
fn page(app: &mut WidgetTestApp, width: f32, height: f32) -> ui_core::node::id::NodeId {
    app.with_cx(|cx| {
        let root = cx.tree().root();
        ui_core::node::builder::NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_PAGE")
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

/// One painted frame: its canvas bounds, its primitives and the lines of text.
struct Frame {
    bounds: Rect,
    primitives: Vec<CanvasPrimitive>,
    texts: Vec<TextRecord>,
}

impl Frame {
    /// The box the chart resolved as its plot.
    fn plot(&self) -> Rect {
        ChartLayout::new(self.bounds).plot
    }

    /// The quads of the frame, in draw order.
    fn quads(&self) -> Vec<CanvasPrimitive> {
        self.primitives
            .iter()
            .copied()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .collect()
    }

    /// The quads of the frame, each with its index in the primitive list (which is what
    /// [`Frame::enclosing_clip`] reads).
    fn quads_indexed(&self) -> Vec<(usize, CanvasPrimitive)> {
        self.primitives
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, primitive)| primitive.kind == PrimitiveKind::Quad)
            .collect()
    }

    /// The label boxes, in draw order, which are the only bordered 20 px tall quads.
    fn label_boxes(&self) -> Vec<CanvasPrimitive> {
        self.quads()
            .into_iter()
            .filter(|quad| {
                let bounds = bounds_of(*quad);
                (bounds.size.height - LABEL_HEIGHT).abs() <= 2.0
                    && bounds.size.width > 20.0
                    && quad.border.is_some()
            })
            .collect()
    }

    /// The box a label's text was drawn inside.
    fn label_box(&self, text: &str) -> CanvasPrimitive {
        let record = self
            .text(text)
            .unwrap_or_else(|| panic!("the frame should draw the label {text:?}"));
        let left = record.origin.x - 12.0;
        let top = record.origin.y;
        self.quads()
            .into_iter()
            .find(|quad| {
                let bounds = bounds_of(*quad);
                (bounds.origin.x - left).abs() <= 2.0
                    && (bounds.origin.y - top).abs() <= 2.0
                    && (bounds.size.height - LABEL_HEIGHT).abs() <= 2.0
            })
            .expect("every drawn label has its box")
    }

    /// One line of drawn text.
    fn text(&self, text: &str) -> Option<&TextRecord> {
        self.texts.iter().find(|record| record.text == text)
    }

    /// Every primitive that draws a registered mesh, with its index in the frame.
    fn geometry_draws(&self) -> Vec<(usize, CanvasPrimitive)> {
        self.primitives
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, primitive)| primitive.kind == PrimitiveKind::Geometry)
            .collect()
    }

    /// The strokes at the series' own colours, which are the lines rather than the
    /// halos.
    fn series_lines(&self) -> Vec<CanvasPrimitive> {
        let palette = palette();
        self.geometry_draws()
            .into_iter()
            .filter(|(_, primitive)| {
                primitive
                    .color
                    .is_some_and(|color| palette.contains(&color))
            })
            .map(|(_, primitive)| primitive)
            .collect()
    }

    /// The clip a primitive at `index` is drawn inside.
    ///
    /// The paint pass never nests clips more than one deep, so the nearest push before
    /// the primitive is its mask.
    fn enclosing_clip(&self, index: usize) -> Option<Rect> {
        self.primitives[..index]
            .iter()
            .rev()
            .find(|primitive| primitive.kind == PrimitiveKind::ClipPush)
            .and_then(|primitive| primitive.bounds)
    }
}

/// A quad's box.
fn bounds_of(primitive: CanvasPrimitive) -> Rect {
    primitive.bounds.expect("a quad has bounds")
}

/// Whether a quad was painted in one of the palette colours at `alpha`.
///
/// The engine's [`Color`] keeps its alpha in the value, so the expected colour is the
/// role resolved by the theme with the same opacity applied.
fn is_faded(quad: CanvasPrimitive, alpha: f32) -> bool {
    palette()
        .iter()
        .any(|role| quad.color == Some(role.with_opacity(alpha)))
}

/// The opacity the pulse ring is drawn at, which is the paint pass's own arithmetic.
fn ring_alpha(phase: f32) -> f32 {
    0.4 * (1.0 - phase)
}

/// The light appearance's six series colours, in palette order.
fn palette() -> [Color; 6] {
    let theme = Theme::of(Appearance::Light);
    [
        theme.role(Role::ChartSeries0),
        theme.role(Role::ChartSeries1),
        theme.role(Role::ChartSeries2),
        theme.role(Role::ChartSeries3),
        theme.role(Role::ChartSeries4),
        theme.role(Role::ChartSeries5),
    ]
}

/// One role in the light appearance.
fn role(role: Role) -> Color {
    Theme::of(Appearance::Light).role(role)
}

fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: expected {expected}, got {actual} (tolerance {tolerance})"
    );
}

/// One point whose probability is `percent` percentage points.
fn point_at(timestamp: i64, percent: f64) -> PredictionPricePoint {
    PredictionPricePoint {
        timestamp,
        probability: Fraction::new(Ratio::from_inner((percent * 1e16).round() as u128))
            .expect("the fixture probability is in range"),
    }
}

/// Two lines, one sample a minute, running from 20 % to the given end values.
///
/// The model keeps the lines in the order the snapshot lists them, so the first line
/// takes the first palette colour and the second the next.
fn two_series(yes_end: f64, no_end: f64) -> Arc<PredictionMarketSnapshot> {
    let event = event_ref();
    let (market, legs) = charted_market(&event, "charted");
    let view = event_with(vec![market.clone()]);
    let line = |end: f64| -> Vec<PredictionPricePoint> {
        (0..=SAMPLE_COUNT)
            .map(|minute| {
                let percent = 20.0 + (end - 20.0) * minute as f64 / SAMPLE_COUNT as f64;
                point_at(minute * MINUTE, percent)
            })
            .collect()
    };
    let series = vec![
        PredictionSeries {
            leg: legs[0].clone(),
            points: line(yes_end),
        },
        PredictionSeries {
            leg: legs[1].clone(),
            points: line(no_end),
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

/// The medium chart these tests draw: the lower line ends at 26 %, the higher at 27 %,
/// so at the middle of the window they are three pixels apart and collide.
fn snapshot() -> Arc<PredictionMarketSnapshot> {
    two_series(26.0, 27.0)
}

/// The fixture probabilities the two lines hold at the middle sample.
fn mid_values() -> (f64, f64) {
    (23.0, 23.5)
}

/// Two lines whose lower one stops at the middle of the window.
///
/// The data grids are normally aligned, so a line that ended earlier is the unusual
/// case the hover has to leave alone past its last sample.
fn snapshot_with_an_ended_line() -> Arc<PredictionMarketSnapshot> {
    let event = event_ref();
    let (market, legs) = charted_market(&event, "charted");
    let view = event_with(vec![market.clone()]);
    let line = |end: f64, last_minute: i64| -> Vec<PredictionPricePoint> {
        (0..=last_minute)
            .map(|minute| {
                let percent = 20.0 + (end - 20.0) * minute as f64 / SAMPLE_COUNT as f64;
                point_at(minute * MINUTE, percent)
            })
            .collect()
    };
    let series = vec![
        PredictionSeries {
            leg: legs[0].clone(),
            points: line(26.0, SAMPLE_COUNT),
        },
        PredictionSeries {
            leg: legs[1].clone(),
            points: line(27.0, SAMPLE_COUNT / 2),
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

/// Twelve lines with distinct values, as a full market draws them.
///
/// The market fixture has two sides, so every line names the same leg; the values are
/// what make the label column twelve boxes tall.
fn twelve_series() -> Arc<PredictionMarketSnapshot> {
    let event = event_ref();
    let (market, legs) = charted_market(&event, "charted");
    let view = event_with(vec![market.clone()]);
    let series = (0..12)
        .map(|index| {
            let end = 20.0 + index as f64 * 5.0;
            PredictionSeries {
                leg: legs[0].clone(),
                points: (0..=SAMPLE_COUNT)
                    .map(|minute| {
                        let percent = 20.0 + (end - 20.0) * minute as f64 / SAMPLE_COUNT as f64;
                        point_at(minute * MINUTE, percent)
                    })
                    .collect(),
            }
        })
        .collect();
    Arc::new(PredictionMarketSnapshot::new(
        1,
        PredictionRange::All,
        view,
        market,
        series,
    ))
}

#[test]
fn a_chart_without_a_hover_draws_no_hover_furniture() {
    let mut harness = ChartHarness::new(snapshot());
    let frame = harness.draw();

    assert!(
        frame.quads().iter().all(
            |quad| !(bounds_of(*quad).size.height > 100.0 && bounds_of(*quad).size.width < 3.0)
        ),
        "no crosshair is drawn without a pointer"
    );
    assert!(
        frame
            .quads()
            .iter()
            .all(|quad| !(11.0..=13.0).contains(&bounds_of(*quad).size.width)),
        "no hover wash is drawn without a pointer; the static endpoint dot is still drawn"
    );
    assert!(frame.label_boxes().is_empty(), "no label column either");
    assert!(
        frame.texts.iter().all(|record| !record.text.contains(", ")),
        "no hovered time is drawn without a pointer"
    );
    assert!(
        frame
            .texts
            .iter()
            .all(|record| !record.text.starts_with("Yes ") && !record.text.starts_with("No ")),
        "no series label is drawn without a pointer"
    );
    assert!(
        frame
            .series_lines()
            .iter()
            .all(|line| line.color == Some(palette()[0]) || line.color == Some(palette()[1])),
        "a line without a hover is at full colour"
    );
}

#[test]
fn a_language_change_relabels_the_axis_and_the_hover() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let english = harness.draw();
    assert!(
        english.text("Jan 1, 12:30 AM").is_some(),
        "the English hovered time is drawn"
    );
    let builds = harness.chart.prepared_builds();

    harness.app.switch_language(LanguageCode::ZhCn);
    let chinese = harness.draw();
    assert!(
        chinese.text("1月1日 00:30").is_some(),
        "the hovered time follows the language"
    );
    assert!(
        chinese.text("00:30").is_some(),
        "the x axis follows the language too"
    );
    assert_eq!(
        harness.chart.prepared_builds(),
        builds + 1,
        "the prepared labels are rebuilt when the language changes"
    );
}

#[test]
fn a_hover_at_either_end_keeps_its_labels_inside_the_plot() {
    let mut harness = ChartHarness::new(two_series(0.0, 100.0));
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.right() - 1.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    let boxes = frame.label_boxes();
    assert_eq!(boxes.len(), 2, "the 0% and 100% labels are both drawn");
    for label in boxes {
        let bounds = bounds_of(label);
        assert!(
            bounds.origin.y >= plot.origin.y - 0.01,
            "a label at the top stays inside the plot: {bounds:?}"
        );
        assert!(
            bounds.bottom() <= plot.bottom() + 0.01,
            "a label at the bottom is not pushed below 0%: {bounds:?}"
        );
    }
}

#[test]
fn the_endpoint_dot_is_as_large_as_a_hovered_intersection() {
    let mut harness = ChartHarness::new(snapshot());
    let frame = harness.draw();
    let plot = frame.plot();
    let endpoints: Vec<CanvasPrimitive> = frame
        .quads()
        .into_iter()
        .filter(|quad| {
            let bounds = bounds_of(*quad);
            (bounds.size.width - 8.0).abs() < 0.01
                && (bounds.origin.x + bounds.size.width / 2.0 - plot.right()).abs() < 1.0
        })
        .collect();

    assert_eq!(endpoints.len(), 2, "one dot per line at the right edge");
}

#[test]
fn the_hover_crosshair_stops_at_the_zero_line() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();
    let crosshair = frame
        .quads()
        .into_iter()
        .find(|quad| {
            let bounds = bounds_of(*quad);
            (bounds.size.width - 1.5).abs() < 0.01 && bounds.size.height > plot.size.height
        })
        .expect("a hover draws the vertical indicator");

    assert!(
        bounds_of(crosshair).bottom() <= plot.bottom() + 0.01,
        "the indicator does not run through the x axis: bottom {} vs 0% {}",
        bounds_of(crosshair).bottom(),
        plot.bottom()
    );
}

#[test]
fn a_mid_chart_hover_stacks_its_labels_on_the_right() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    let (lower, higher) = mid_values();
    // The axis covers the whole snapshot: the higher line ends at 27 %, which rounds
    // the top of the axis to 30.
    let axis = ValueAxis::new(27.0);
    let crosshair_x = plot.origin.x + plot.size.width / 2.0;
    let lower_y = y_for_percent(lower, &axis, plot);
    let higher_y = y_for_percent(higher, &axis, plot);

    let boxes = frame.label_boxes();
    assert_eq!(boxes.len(), 2, "one box per line");
    assert!(
        boxes
            .iter()
            .all(|quad| (bounds_of(*quad).origin.x - (crosshair_x + 8.0)).abs() <= 2.0),
        "a label column that fits opens to the right, at the crosshair plus eight"
    );

    let mut sorted = boxes;
    sorted.sort_by(|left, right| {
        bounds_of(*left)
            .origin
            .y
            .total_cmp(&bounds_of(*right).origin.y)
    });
    let uppermost = bounds_of(sorted[0]);
    let lower_box = bounds_of(sorted[1]);

    assert_close(
        lower_box.bottom(),
        lower_y,
        2.0,
        "the lowest box's bottom sits on its own point",
    );
    assert_close(
        uppermost.bottom(),
        lower_box.origin.y - LABEL_GAP,
        2.0,
        "the box above is pushed up to a one-pixel gap",
    );
    assert!(
        uppermost.bottom() < higher_y,
        "a collided box is above its natural point: {} vs {higher_y}",
        uppermost.bottom()
    );

    let lower_label = frame
        .text("Yes 23.0%")
        .expect("the lower line's label names it");
    assert_close(
        bounds_of(frame.label_box("Yes 23.0%")).size.width,
        lower_label.shaped_width + 18.0,
        2.0,
        "the box is its text plus its padding, bar and gap",
    );
    assert!(
        frame.text("No 23.5%").is_some(),
        "every line with a value at the sample is named"
    );
}

#[test]
fn a_twelve_series_column_keeps_every_label_at_a_distinct_top() {
    // Twelve 20 px boxes plus their 1 px gaps are taller than the plot, so the
    // column has to overflow without piling boxes on top of one another.
    let mut harness = ChartHarness::sized(twelve_series(), WINDOW_WIDTH, WINDOW_HEIGHT + 100.0);
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    let mut tops: Vec<f32> = frame
        .label_boxes()
        .iter()
        .map(|quad| bounds_of(*quad).origin.y)
        .collect();
    assert_eq!(tops.len(), 12, "one box per line");
    tops.sort_by(f32::total_cmp);
    assert!(
        tops.windows(2).all(|pair| pair[1] > pair[0]),
        "twelve boxes keep twelve distinct tops: {tops:?}"
    );
    assert!(
        tops[0] < plot.origin.y,
        "the overflow leaves the plot above 100%: {tops:?}"
    );
    assert!(
        tops[tops.len() - 1] + LABEL_HEIGHT <= plot.bottom(),
        "the lowest box stays inside the plot: {tops:?}"
    );
}

#[test]
fn a_hover_near_the_right_edge_flips_the_label_column_left() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    let last_sample_x = plot.origin.x + plot.size.width * 59.0 / 60.0;
    harness.set_pointer(Some(Point::new(
        last_sample_x,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    let boxes = frame.label_boxes();
    assert_eq!(boxes.len(), 2);
    for quad in boxes {
        let bounds = bounds_of(quad);
        assert_close(
            bounds.right(),
            last_sample_x - 8.0,
            2.0,
            "a flipped column keeps its right edge eight pixels left of the crosshair",
        );
        assert!(
            bounds.origin.x >= plot.origin.x,
            "a flipped box stays inside the plot"
        );
    }
}

#[test]
fn hovering_fades_every_line_to_the_right_of_the_sample() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();
    let crosshair_x = plot.origin.x + plot.size.width / 2.0;

    for (index, color) in palette().iter().enumerate().take(2) {
        let mut strokes: Vec<(usize, CanvasPrimitive)> = frame
            .geometry_draws()
            .into_iter()
            .filter(|(_, primitive)| {
                primitive.color == Some(*color) || primitive.color == Some(color.with_opacity(0.25))
            })
            .collect();
        assert_eq!(
            strokes.len(),
            2,
            "each line is painted once per side (line {index})"
        );
        // The faded half is painted first, so the full-colour half is on top of it.
        strokes.sort_by_key(|(_, primitive)| primitive.color == Some(*color));
        let (faded_index, faded) = strokes[0];
        let (full_index, full) = strokes[1];
        assert_eq!(
            faded.color,
            Some(color.with_opacity(0.25)),
            "the right side is faded to a quarter"
        );
        assert_eq!(full.color, Some(*color), "the left side is at full colour");
        let faded_clip = frame
            .enclosing_clip(faded_index)
            .expect("the faded stroke is clipped");
        let full_clip = frame
            .enclosing_clip(full_index)
            .expect("the full stroke is clipped");
        assert!(
            faded_clip.origin.x >= crosshair_x - 1.0,
            "the faded mesh is clipped to the right of the crosshair"
        );
        assert!(
            full_clip.right() <= crosshair_x + 1.0,
            "the full mesh is clipped to the left of the crosshair"
        );
    }
}

#[test]
fn a_pulse_starts_outside_the_endpoint_dot() {
    let mut harness = ChartHarness::new(snapshot());
    harness.draw();

    harness.set_pulse(Some(0.0));
    let frame = harness.draw();
    let ring = frame
        .quads()
        .into_iter()
        .find(|quad| {
            quad.color == Some(palette()[0].with_opacity(ring_alpha(0.0)))
                && (bounds_of(*quad).size.width - 12.0).abs() < 0.01
        })
        .expect("the pulse starts as a ring wider than the endpoint dot");

    assert_eq!(
        bounds_of(ring).size.width,
        12.0,
        "a six-pixel radius around a four-pixel dot"
    );
}

#[test]
fn a_pulse_phase_draws_the_expanding_ring() {
    let mut harness = ChartHarness::new(snapshot());
    harness.draw();

    harness.set_pulse(Some(0.25));
    let early = harness.draw();
    // The ring at a quarter of its cycle: radius 7, and a quarter of the opacity gone.
    let early_ring = early
        .quads()
        .into_iter()
        .filter(|quad| {
            (bounds_of(*quad).size.width - 14.0).abs() < 0.01 && is_faded(*quad, ring_alpha(0.25))
        })
        .count();
    assert_eq!(early_ring, 2, "one ring around each line's endpoint");

    harness.set_pulse(Some(0.75));
    let late = harness.draw();
    let widest = late
        .quads()
        .into_iter()
        .filter(|quad| is_faded(*quad, ring_alpha(0.75)))
        .map(|quad| bounds_of(quad).size.width)
        .fold(0.0_f32, f32::max);
    assert!(
        widest > 13.0,
        "the ring expands as the phase advances: {widest}"
    );
}

#[test]
fn no_pulse_phase_draws_no_ring() {
    let mut harness = ChartHarness::new(snapshot());
    let frame = harness.draw();

    assert!(
        frame
            .quads()
            .iter()
            .all(|quad| !is_faded(*quad, ring_alpha(0.0))),
        "without a phase the endpoint is a plain dot"
    );
}

#[test]
fn the_hovered_time_sits_in_the_top_strip() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();
    let crosshair_x = plot.origin.x + plot.size.width / 2.0;

    let label = frame
        .text("Jan 1, 12:30 AM")
        .expect("the hovered sample's time is drawn");
    assert_close(
        label.origin.x,
        crosshair_x + 8.0,
        2.0,
        "the time is left-aligned eight pixels right of the crosshair",
    );
    assert!(
        label.origin.y < plot.origin.y,
        "the time is drawn above the plot"
    );
    assert_eq!(label.font_size, 12.0);
}

#[test]
fn a_hover_near_the_right_edge_flips_the_time_label_left() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    let last_sample_x = plot.origin.x + plot.size.width * 59.0 / 60.0;
    harness.set_pointer(Some(Point::new(
        last_sample_x,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    let label = frame
        .text("Jan 1, 12:59 AM")
        .expect("the hovered sample's time is drawn");
    assert!(
        label.origin.x + label.shaped_width <= last_sample_x - 8.0 + 2.0,
        "the time flips to the left of the crosshair when it would overflow"
    );
    assert!(label.origin.x >= 0.0, "and it stays inside the element");
}

#[test]
fn a_pointer_only_frame_reuses_the_prepared_chart() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    let prepared = harness.chart.prepared_builds();
    assert!(prepared >= 1, "a drawn chart has prepared a frame");

    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    harness.draw();
    assert_eq!(
        harness.chart.prepared_builds(),
        prepared,
        "a pointer that moved does not rebuild the geometry"
    );

    harness.snapshot = Arc::new((*harness.snapshot).clone());
    harness.draw();
    assert_eq!(
        harness.chart.prepared_builds(),
        prepared + 1,
        "a new snapshot does rebuild it"
    );
}

#[test]
fn the_vertical_grid_starts_below_the_time_strip_and_reaches_the_bottom_band() {
    let mut harness = ChartHarness::new(snapshot());
    let frame = harness.draw();
    let plot = frame.plot();
    let grid = role(Role::ChartGrid);
    let dots: Vec<CanvasPrimitive> = frame
        .quads()
        .into_iter()
        .filter(|quad| {
            let bounds = bounds_of(*quad);
            quad.color == Some(grid) && bounds.size.width <= 1.5 && bounds.size.height <= 1.5
        })
        .collect();
    assert!(!dots.is_empty(), "the grid is drawn");

    let top = dots
        .iter()
        .map(|dot| bounds_of(*dot).origin.y)
        .fold(f32::INFINITY, f32::min);
    assert!(
        top >= plot.origin.y - 0.01,
        "the vertical grid stops at the plot's top instead of running through the time \
         strip: top {top} vs plot top {}",
        plot.origin.y
    );

    let bottom = dots
        .iter()
        .map(|dot| bounds_of(*dot).bottom())
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        bottom > plot.bottom(),
        "the vertical grid still runs through the x-label band: bottom {bottom}"
    );
}

#[test]
fn a_hover_dot_on_the_left_plot_edge_is_not_cut_by_the_plot_mask() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + 0.5,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    let (index, dot) = frame
        .quads_indexed()
        .into_iter()
        .find(|(_, quad)| {
            (bounds_of(*quad).size.width - 12.0).abs() <= 0.01 && is_faded(*quad, 0.1)
        })
        .expect("the hover dot's wash is drawn at the first sample");
    let left = bounds_of(dot).origin.x;
    let plot_left = plot.origin.x;

    assert_close(
        left,
        plot_left - 6.0,
        0.01,
        "the wash around a dot centred on the plot's left edge reaches six pixels past it",
    );
    let mask = frame
        .enclosing_clip(index)
        .expect("the dot is drawn in a mask");
    assert!(
        mask.origin.x <= left + 0.01,
        "the dot mask keeps the whole wash visible: mask left {} vs dot left {left}",
        mask.origin.x
    );
}

#[test]
fn a_hover_dot_on_the_plot_edge_is_not_cut_by_the_plot_mask() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.right() - 1.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    let (index, dot) = frame
        .quads_indexed()
        .into_iter()
        .find(|(_, quad)| {
            (bounds_of(*quad).size.width - 12.0).abs() <= 0.01 && is_faded(*quad, 0.1)
        })
        .expect("the hover dot's wash is drawn at the last sample");
    let right = bounds_of(dot).right();
    let plot_right = plot.right();

    assert!(
        right > plot_right,
        "the dot is centred on the plot's right edge, so its wash reaches past it: \
         {right} vs {plot_right}"
    );
    let mask = frame
        .enclosing_clip(index)
        .expect("the dot is drawn in a mask");
    assert!(
        mask.right() >= right - 0.01,
        "the dot mask keeps the whole wash visible: mask right {} vs dot right {right}",
        mask.right()
    );
    let label = frame.text("10%").expect("the axis labels a tick");
    assert!(
        mask.right() < label.origin.x,
        "the dot mask stays left of the y-label column"
    );
}

#[test]
fn the_endpoint_pulse_reaches_past_the_plot_edge_without_touching_the_labels() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pulse(Some(0.95));
    let frame = harness.draw();

    let (index, ring_quad) = frame
        .quads_indexed()
        .into_iter()
        .filter(|(_, quad)| is_faded(*quad, ring_alpha(0.95)))
        .max_by(|left, right| {
            bounds_of(left.1)
                .size
                .width
                .total_cmp(&bounds_of(right.1).size.width)
        })
        .expect("the pulse ring is drawn");
    let ring = bounds_of(ring_quad);
    assert!(
        ring.size.width > 16.0,
        "a late phase draws the widest ring: {}",
        ring.size.width
    );

    let plot_right = plot.right();
    assert!(
        ring.right() > plot_right,
        "the pulse is centred on the plot's right edge, so it reaches past it: \
         {} vs {plot_right}",
        ring.right()
    );
    let mask = frame
        .enclosing_clip(index)
        .expect("the pulse is drawn in a mask");
    assert!(
        mask.right() >= ring.right() - 0.01,
        "the dot mask keeps the whole pulse visible: mask right {} vs ring right {}",
        mask.right(),
        ring.right()
    );

    let label = frame.text("30%").expect("the axis labels its top tick");
    assert!(
        ring.right() <= label.origin.x,
        "the pulse stays left of the y-label column: {} vs {}",
        ring.right(),
        label.origin.x
    );
}

#[test]
fn an_endpoint_pulse_at_the_axis_top_is_not_cut_by_its_mask() {
    let mut harness = ChartHarness::new(two_series(100.0, 50.0));
    let plot = harness.draw().plot();
    harness.set_pulse(Some(0.95));
    let frame = harness.draw();

    let (index, ring_quad) = frame
        .quads_indexed()
        .into_iter()
        .filter(|(_, quad)| {
            is_faded(*quad, ring_alpha(0.95)) && bounds_of(*quad).origin.y < plot.origin.y
        })
        .max_by(|left, right| {
            bounds_of(left.1)
                .size
                .width
                .total_cmp(&bounds_of(right.1).size.width)
        })
        .expect("the pulse ring at the axis top is drawn");
    let ring = bounds_of(ring_quad);
    let plot_top = plot.origin.y;

    assert!(
        ring.origin.y < plot_top - 6.0,
        "the ring reaches past the hover dot's wash at the axis top: {} vs {plot_top}",
        ring.origin.y
    );
    let mask = frame
        .enclosing_clip(index)
        .expect("the pulse is drawn in a mask");
    assert!(
        mask.origin.y <= ring.origin.y + 0.01,
        "the endpoint mask keeps the ring's top visible: mask top {} vs ring top {}",
        mask.origin.y,
        ring.origin.y
    );
}

#[test]
fn the_y_labels_fit_the_narrowed_gutter() {
    let mut harness = ChartHarness::new(two_series(100.0, 50.0));
    let frame = harness.draw();
    let plot = frame.plot();

    let label = frame
        .text("100%")
        .expect("a chart at certainty labels its hundred-percent tick");
    let right = label.origin.x + label.shaped_width;
    assert!(
        right <= frame.bounds.right() + 0.01,
        "the widest y label still fits the gutter: text right {right}, bounds right {}",
        frame.bounds.right()
    );
    assert!(
        label.origin.x >= plot.right() + 9.0,
        "the y labels start clear of the dot mask"
    );
}

#[test]
fn a_line_that_ended_before_the_hover_grows_no_dot_or_label() {
    let mut harness = ChartHarness::new(snapshot_with_an_ended_line());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.right() - 1.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    assert_eq!(
        frame.label_boxes().len(),
        1,
        "only the line that reaches the hovered sample is named"
    );
    assert!(
        frame
            .texts
            .iter()
            .all(|record| !record.text.starts_with("No ")),
        "the line that stopped halfway grows no label at a later x"
    );
    assert!(
        frame.text("Yes 26.0%").is_some(),
        "the line that still covers the sample keeps its label"
    );

    let washes = frame
        .quads()
        .into_iter()
        .filter(|quad| (bounds_of(*quad).size.width - 12.0).abs() <= 0.01 && is_faded(*quad, 0.1))
        .count();
    assert_eq!(
        washes, 1,
        "only the line that covers the sample grows a hover dot"
    );
}

/// Every series line and its halo is one registered mesh; a two-line chart therefore
/// names four meshes, and the halo is drawn under its line in the body's own colour.
#[test]
fn the_frame_draws_one_mesh_per_line_and_a_halo_under_it() {
    let mut harness = ChartHarness::new(snapshot());
    let frame = harness.draw();

    let meshes = frame.geometry_draws();
    assert_eq!(meshes.len(), 4, "two lines, each with its halo: {meshes:?}");
    let halo = role(Role::SurfaceSunken);
    assert_eq!(
        meshes
            .iter()
            .filter(|(_, primitive)| primitive.color == Some(halo))
            .count(),
        2,
        "one halo per line, in the pane body's colour"
    );
    assert!(
        meshes
            .iter()
            .all(|(_, primitive)| primitive.geometry.is_some()),
        "each draw names a registered mesh"
    );
}

/// Every clip the frame opens is closed again: an unbalanced bracket would leak one
/// series' mask into the primitives after it.
#[test]
fn every_clip_the_frame_opens_is_closed() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    harness.set_pulse(Some(0.5));
    let frame = harness.draw();

    let mut depth: i32 = 0;
    for primitive in &frame.primitives {
        match primitive.kind {
            PrimitiveKind::ClipPush => depth += 1,
            PrimitiveKind::ClipPop => {
                depth -= 1;
                assert!(depth >= 0, "a clip was popped before it was pushed");
            }
            _ => {}
        }
    }
    assert_eq!(depth, 0, "every clip the frame opened was closed");
}

/// A resize lays the chart out again: the canvas is rebuilt for the new box, and its
/// plot is the new box's.
#[test]
fn a_resize_redraws_the_chart_in_the_new_box() {
    let mut harness = ChartHarness::new(snapshot());
    let before = harness.draw().bounds;

    harness.resize(WINDOW_WIDTH, WINDOW_HEIGHT + 100.0);
    let frame = harness.draw();

    assert_eq!(
        frame.bounds.size.height,
        before.size.height + 100.0,
        "the canvas covers the box the node was given"
    );
    assert!(
        frame.plot().size.height > ChartLayout::new(before).plot.size.height,
        "the plot grew with the box"
    );
}

/// The lines the chart's canvas names are registered for as long as it draws them: a
/// pointer-only frame must not hand its meshes back and draw from freed geometry.
#[test]
fn a_pointer_move_keeps_the_meshes_the_canvas_names() {
    let mut harness = ChartHarness::new(snapshot());
    let plot = harness.draw().plot();
    let registered = harness.app.app().geometry().len();
    assert_eq!(registered, 4, "two lines and their halos");

    harness.set_pointer(Some(Point::new(
        plot.origin.x + plot.size.width / 2.0,
        plot.origin.y + plot.size.height / 2.0,
    )));
    let frame = harness.draw();

    assert_eq!(
        harness.app.app().geometry().len(),
        registered,
        "a pointer-only frame registers no geometry"
    );
    assert_eq!(
        frame.geometry_draws().len(),
        6,
        "a hovered frame draws each line in two halves plus its halo"
    );
}

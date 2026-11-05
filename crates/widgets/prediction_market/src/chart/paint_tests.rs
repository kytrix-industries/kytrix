//! What a frame of the prediction chart actually draws, and what it reuses.
//!
//! A chart is a canvas, so two of its three outputs are invisible to a test: glyphs
//! and stroked paths live in a private scene. The paint pass therefore reports what
//! it drew into a small test-only recorder, and quads are read back from the window
//! itself. What is pinned here is the hover's whole shape — the crosshair's place, the
//! fade split, the label column's side and stack, the time label — plus the cache
//! rule that a pointer-only frame prepares nothing.
//!
//! There is no pixel capture: on Windows the test window has no headless renderer, so
//! a PNG is not possible. These are geometry and paint-call assertions.

use std::sync::Arc;

use base_ui::style::theme::ThemeExt;
use gpui_kit::{
    Bounds, Context, Entity, Hsla, IntoElement, ParentElement, Pixels, Point, Render, ScaledPixels,
    Size, Styled, TestAppContext, VisualTestContext, Window, div, point, px, size,
};
use i18n::language::codes::LanguageCode;
use market_participants_model::{
    decimal::{Fraction, Ratio},
    prediction::{PredictionPricePoint, PredictionRange, PredictionSeries},
};

use super::{
    canvas::{ChartCanvasOptions, ChartPaintCaches, chart_element},
    geometry::ChartLayout,
    hover::LABEL_HEIGHT,
    paint::{PaintLog, reset_log, take_log},
};
use crate::{
    model::PredictionMarketSnapshot,
    test_support::{charted_market, event_ref, event_with},
};

/// The window the chart is mounted in: the reference chart's 880 x 200 plot plus its
/// strips (48 px across, 48 px down).
const WINDOW_WIDTH: f32 = 928.0;
const WINDOW_HEIGHT: f32 = 248.0;

/// One sample per minute over an hour.
const SAMPLE_COUNT: i64 = 60;
const MINUTE: i64 = 60_000;
/// The step the label boxes keep from each other.
const LABEL_GAP: f32 = 1.0;

/// One chart mounted as the pane mounts it.
struct Harness {
    snapshot: Arc<PredictionMarketSnapshot>,
    options: ChartCanvasOptions,
    caches: std::rc::Rc<ChartPaintCaches>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(chart_element(
            self.snapshot.clone(),
            self.options.clone(),
            self.caches.clone(),
        ))
    }
}

/// One painted frame: its box, its quads in logical pixels, and the paint log.
struct Frame {
    bounds: Bounds<Pixels>,
    quads: Vec<PaintedQuad>,
    log: PaintLog,
}

impl Frame {
    /// The box the chart resolved as its plot.
    fn plot(&self) -> Bounds<Pixels> {
        ChartLayout::new(self.bounds).plot
    }

    /// The label boxes, in draw order, which are the only 20 px tall quads.
    fn label_boxes(&self) -> Vec<&PaintedQuad> {
        self.quads
            .iter()
            .filter(|quad| {
                (f32::from(quad.bounds.size.height) - 20.0).abs() <= 2.0
                    && f32::from(quad.bounds.size.width) > 20.0
                    && quad.border_top > Pixels::ZERO
            })
            .collect()
    }

    /// The box a label's text was drawn inside.
    fn label_box(&self, text: &str) -> &PaintedQuad {
        let record = self
            .text(text)
            .unwrap_or_else(|| panic!("the frame should draw the label {text:?}"));
        let left = f32::from(record.origin.x) - 12.0;
        let top = f32::from(record.origin.y);
        self.quads
            .iter()
            .find(|quad| {
                (f32::from(quad.bounds.origin.x) - left).abs() <= 2.0
                    && (f32::from(quad.bounds.origin.y) - top).abs() <= 2.0
                    && (f32::from(quad.bounds.size.height) - 20.0).abs() <= 2.0
            })
            .expect("every drawn label has its box")
    }

    /// One line of drawn text.
    fn text(&self, text: &str) -> Option<&super::paint::TextRecord> {
        self.log.texts.iter().find(|record| record.text == text)
    }

    /// The strokes at the series' own width, which are the lines rather than the
    /// halos.
    fn series_strokes(&self) -> impl Iterator<Item = &super::paint::StrokeRecord> {
        self.log
            .strokes
            .iter()
            .filter(|stroke| (stroke.width - 1.75).abs() < 0.01)
    }
}

/// One quad as a test reads it: logical-pixel bounds, the mask it is clipped to, and
/// the fill.
struct PaintedQuad {
    bounds: Bounds<Pixels>,
    mask: Bounds<Pixels>,
    color: Option<Hsla>,
    border_top: Pixels,
}

impl PaintedQuad {
    /// The width of the quad in logical pixels.
    fn width(&self) -> f32 {
        f32::from(self.bounds.size.width)
    }

    /// The height of the quad in logical pixels.
    fn height(&self) -> f32 {
        f32::from(self.bounds.size.height)
    }

    /// The alpha the quad was filled with, when its fill is a solid colour.
    fn alpha(&self) -> Option<f32> {
        self.color.map(|color| color.a)
    }
}

/// Mounts one chart at the fixed size the reference chart measures.
fn chart_window(
    cx: &mut TestAppContext,
    snapshot: Arc<PredictionMarketSnapshot>,
) -> (Entity<Harness>, &mut VisualTestContext) {
    chart_window_sized(cx, snapshot, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)))
}

/// Mounts one chart in a window of the given size.
fn chart_window_sized(
    cx: &mut TestAppContext,
    snapshot: Arc<PredictionMarketSnapshot>,
    window_size: Size<Pixels>,
) -> (Entity<Harness>, &mut VisualTestContext) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
    });
    let (harness, cx) = cx.add_window_view(|_, _| Harness {
        snapshot,
        options: ChartCanvasOptions::default(),
        caches: std::rc::Rc::new(ChartPaintCaches::default()),
    });
    cx.simulate_resize(window_size);
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    (harness, cx)
}

/// Draws the frame the current options ask for and reads it back.
fn draw(harness: &Entity<Harness>, cx: &mut VisualTestContext) -> Frame {
    reset_log();
    cx.update(|window, cx| {
        harness.update(cx, |_, cx| cx.notify());
        window.refresh();
    });
    cx.run_until_parked();
    let (bounds, scale, quads) = cx.update(|window, _| {
        (
            window.bounds(),
            window.scale_factor(),
            window.painted_quads(),
        )
    });
    let quads = quads
        .into_iter()
        .map(|quad| PaintedQuad {
            bounds: logical_bounds(quad.bounds, scale),
            mask: logical_bounds(quad.content_mask.bounds, scale),
            color: quad.background.as_solid(),
            border_top: px(quad.border_widths.top.0 / scale),
        })
        .collect();
    Frame {
        bounds,
        quads,
        log: take_log(),
    }
}

/// A scaled-pixel rect from the window's scene in logical pixels.
fn logical_bounds(bounds: Bounds<ScaledPixels>, scale: f32) -> Bounds<Pixels> {
    let logical = |value: f32| px(value / scale);
    Bounds::new(
        point(logical(bounds.origin.x.0), logical(bounds.origin.y.0)),
        size(logical(bounds.size.width.0), logical(bounds.size.height.0)),
    )
}

/// Points the chart at a window-space position for the next frame.
fn set_pointer(
    harness: &Entity<Harness>,
    cx: &mut VisualTestContext,
    pointer: Option<Point<Pixels>>,
) {
    cx.update(|_, cx| {
        harness.update(cx, |harness, _| harness.options.pointer = pointer);
    });
}

/// Starts or stops the endpoint pulse for the next frame.
fn set_pulse(harness: &Entity<Harness>, cx: &mut VisualTestContext, phase: Option<f32>) {
    cx.update(|_, cx| {
        harness.update(cx, |harness, _| harness.options.pulse_phase = phase);
    });
}

/// The chart's caches, which the cache test reads back.
fn caches(harness: &Entity<Harness>, cx: &mut VisualTestContext) -> std::rc::Rc<ChartPaintCaches> {
    cx.update(|_, cx| harness.read(cx).caches.clone())
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

#[gpui_kit::test]
fn a_chart_without_a_hover_draws_no_hover_furniture(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let frame = draw(&harness, cx);

    assert!(
        frame
            .quads
            .iter()
            .all(|quad| !(quad.height() > 100.0 && quad.width() < 3.0)),
        "no crosshair is drawn without a pointer"
    );
    assert!(
        frame
            .quads
            .iter()
            .all(|quad| !(11.0..=13.0).contains(&quad.width())),
        "no hover wash is drawn without a pointer; the static endpoint dot is still drawn"
    );
    assert!(frame.label_boxes().is_empty(), "no label column either");
    assert!(
        frame
            .log
            .texts
            .iter()
            .all(|record| !record.text.contains(", ")),
        "no hovered time is drawn without a pointer"
    );
    assert!(
        frame
            .log
            .texts
            .iter()
            .all(|record| !record.text.starts_with("Yes ") && !record.text.starts_with("No ")),
        "no series label is drawn without a pointer"
    );
    assert!(
        frame
            .series_strokes()
            .all(|stroke| (stroke.color.a - 1.0).abs() < 0.01),
        "a line without a hover is at full colour"
    );
}

#[gpui_kit::test]
fn a_language_change_relabels_the_axis_and_the_hover(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(&harness, cx, Some(plot.center()));
    let english = draw(&harness, cx);
    assert!(
        english.text("Jan 1, 12:30 AM").is_some(),
        "the English hovered time is drawn"
    );
    let caches = caches(&harness, cx);
    let builds = caches.prepared_builds();

    cx.update(|_, cx| cx.set_global(LanguageCode::ZhCn));
    let chinese = draw(&harness, cx);
    assert!(
        chinese.text("1月1日 00:30").is_some(),
        "the hovered time follows the language"
    );
    assert!(
        chinese.text("00:30").is_some(),
        "the x axis follows the language too"
    );
    assert_eq!(
        caches.prepared_builds(),
        builds + 1,
        "the prepared labels are rebuilt when the language changes"
    );
}

#[gpui_kit::test]
fn a_hover_at_either_end_keeps_its_labels_inside_the_plot(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, two_series(0.0, 100.0));
    let plot = draw(&harness, cx).plot();
    set_pointer(
        &harness,
        cx,
        Some(point(plot.right() - px(1.0), plot.center().y)),
    );
    let frame = draw(&harness, cx);

    let boxes = frame.label_boxes();
    assert_eq!(boxes.len(), 2, "the 0% and 100% labels are both drawn");
    for label in boxes {
        assert!(
            f32::from(label.bounds.origin.y) >= f32::from(plot.origin.y) - 0.01,
            "a label at the top stays inside the plot: {:?}",
            label.bounds
        );
        assert!(
            f32::from(label.bounds.bottom()) <= f32::from(plot.bottom()) + 0.01,
            "a label at the bottom is not pushed below 0%: {:?}",
            label.bounds
        );
    }
}

#[gpui_kit::test]
fn the_endpoint_dot_is_as_large_as_a_hovered_intersection(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let frame = draw(&harness, cx);
    let plot = frame.plot();
    let endpoints: Vec<&PaintedQuad> = frame
        .quads
        .iter()
        .filter(|quad| {
            (quad.width() - 8.0).abs() < 0.01
                && (f32::from(quad.bounds.origin.x + quad.bounds.size.width / 2.0)
                    - f32::from(plot.right()))
                .abs()
                    < 1.0
        })
        .collect();

    assert_eq!(endpoints.len(), 2, "one dot per line at the right edge");
}

#[gpui_kit::test]
fn the_hover_crosshair_stops_at_the_zero_line(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(&harness, cx, Some(plot.center()));
    let frame = draw(&harness, cx);
    let crosshair = frame
        .quads
        .iter()
        .find(|quad| {
            (quad.width() - 1.5).abs() < 0.01 && quad.height() > f32::from(plot.size.height)
        })
        .expect("a hover draws the vertical indicator");

    assert!(
        f32::from(crosshair.bounds.bottom()) <= f32::from(plot.bottom()) + 0.01,
        "the indicator does not run through the x axis: bottom {} vs 0% {}",
        f32::from(crosshair.bounds.bottom()),
        f32::from(plot.bottom())
    );
}

#[gpui_kit::test]
fn a_mid_chart_hover_stacks_its_labels_on_the_right(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(&harness, cx, Some(plot.center()));
    let frame = draw(&harness, cx);

    let (lower, higher) = mid_values();
    // The axis covers the whole snapshot: the higher line ends at 27 %, which rounds
    // the top of the axis to 30.
    let axis = super::geometry::ValueAxis::new(27.0);
    let crosshair_x = f32::from(plot.origin.x) + f32::from(plot.size.width) / 2.0;
    let lower_y = f32::from(super::geometry::y_for_percent(lower, &axis, plot));
    let higher_y = f32::from(super::geometry::y_for_percent(higher, &axis, plot));

    let boxes = frame.label_boxes();
    assert_eq!(boxes.len(), 2, "one box per line");
    assert!(
        boxes
            .iter()
            .all(|quad| (f32::from(quad.bounds.origin.x) - (crosshair_x + 8.0)).abs() <= 2.0),
        "a label column that fits opens to the right, at the crosshair plus eight"
    );

    let mut sorted = boxes.clone();
    sorted.sort_by(|left, right| {
        f32::from(left.bounds.origin.y).total_cmp(&f32::from(right.bounds.origin.y))
    });
    let uppermost = sorted[0];
    let lower_box = sorted[1];
    let lower_bottom = f32::from(lower_box.bounds.bottom());
    let upper_bottom = f32::from(uppermost.bounds.bottom());

    assert_close(
        lower_bottom,
        lower_y,
        2.0,
        "the lowest box's bottom sits on its own point",
    );
    assert_close(
        upper_bottom,
        f32::from(lower_box.bounds.origin.y) - LABEL_GAP,
        2.0,
        "the box above is pushed up to a one-pixel gap",
    );
    assert!(
        upper_bottom < higher_y,
        "a collided box is above its natural point: {upper_bottom} vs {higher_y}"
    );

    let lower_label = frame
        .text("Yes 23.0%")
        .expect("the lower line's label names it");
    assert_close(
        f32::from(frame.label_box("Yes 23.0%").bounds.size.width),
        f32::from(lower_label.width) + 12.0,
        2.0,
        "the box is its text plus its padding, bar and gap",
    );
    assert!(
        frame.text("No 23.5%").is_some(),
        "every line with a value at the sample is named"
    );
}

#[gpui_kit::test]
fn a_twelve_series_column_keeps_every_label_at_a_distinct_top(cx: &mut TestAppContext) {
    // Twelve 20 px boxes plus their 1 px gaps are taller than the plot, so the
    // column has to overflow without piling boxes on top of one another.
    let (harness, cx) = chart_window_sized(
        cx,
        twelve_series(),
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT + 100.0)),
    );
    let plot = draw(&harness, cx).plot();
    set_pointer(&harness, cx, Some(plot.center()));
    let frame = draw(&harness, cx);

    let mut tops: Vec<f32> = frame
        .label_boxes()
        .iter()
        .map(|quad| f32::from(quad.bounds.origin.y))
        .collect();
    assert_eq!(tops.len(), 12, "one box per line");
    tops.sort_by(f32::total_cmp);
    assert!(
        tops.windows(2).all(|pair| pair[1] > pair[0]),
        "twelve boxes keep twelve distinct tops: {tops:?}"
    );
    assert!(
        tops[0] < f32::from(plot.origin.y),
        "the overflow leaves the plot above 100%: {tops:?}"
    );
    assert!(
        tops[tops.len() - 1] + LABEL_HEIGHT <= f32::from(plot.bottom()),
        "the lowest box stays inside the plot: {tops:?}"
    );
}

#[gpui_kit::test]
fn a_hover_near_the_right_edge_flips_the_label_column_left(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    let pointer = point(plot.right() - px(20.0), plot.center().y);
    set_pointer(&harness, cx, Some(pointer));
    let frame = draw(&harness, cx);

    // The pointer snaps to the fifty-ninth minute, whose x is the plot's right edge
    // less one sample step.
    let crosshair_x = f32::from(plot.origin.x) + f32::from(plot.size.width) * 59.0 / 60.0;
    let boxes = frame.label_boxes();
    assert_eq!(boxes.len(), 2);
    for quad in boxes {
        let right = f32::from(quad.bounds.right());
        assert_close(
            right,
            crosshair_x - 8.0,
            2.0,
            "a flipped column keeps its right edge eight pixels left of the crosshair",
        );
        assert!(
            f32::from(quad.bounds.origin.x) >= f32::from(plot.origin.x),
            "a flipped box stays inside the plot"
        );
    }
}

#[gpui_kit::test]
fn hovering_fades_every_line_to_the_right_of_the_sample(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(&harness, cx, Some(plot.center()));
    let frame = draw(&harness, cx);
    let crosshair_x = f32::from(plot.origin.x) + f32::from(plot.size.width) / 2.0;

    for index in 0..2 {
        let color = cx.update(|_, cx| cx.theme().roles.chart.series[index].hsla);
        let mut strokes: Vec<_> = frame
            .series_strokes()
            .filter(|stroke| (stroke.color.h - color.h).abs() < f32::EPSILON)
            .collect();
        strokes.sort_by(|left, right| left.color.a.total_cmp(&right.color.a));
        assert_eq!(strokes.len(), 2, "each line is painted once per side");
        let faded = strokes[0];
        let full = strokes[1];
        assert_close(
            faded.color.a,
            0.25,
            0.01,
            "the right side is faded to a quarter",
        );
        assert_close(full.color.a, 1.0, 0.01, "the left side is at full colour");
        assert!(
            f32::from(faded.mask.origin.x) >= crosshair_x - 1.0,
            "the faded path is clipped to the right of the crosshair"
        );
        assert!(
            f32::from(full.mask.right()) <= crosshair_x + 1.0,
            "the full path is clipped to the left of the crosshair"
        );
    }
}

#[gpui_kit::test]
fn a_pulse_starts_outside_the_endpoint_dot(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    draw(&harness, cx);

    set_pulse(&harness, cx, Some(0.0));
    let frame = draw(&harness, cx);
    let ring = frame
        .quads
        .iter()
        .find(|quad| {
            quad.alpha().is_some_and(|alpha| (alpha - 0.4).abs() < 0.01)
                && (quad.width() - 12.0).abs() < 0.01
        })
        .expect("the pulse starts as a ring wider than the endpoint dot");

    assert_eq!(
        ring.width(),
        12.0,
        "a six-pixel radius around a four-pixel dot"
    );
}

#[gpui_kit::test]
fn a_pulse_phase_draws_the_expanding_ring(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    draw(&harness, cx);

    set_pulse(&harness, cx, Some(0.25));
    let early = draw(&harness, cx);
    let early_ring = early
        .quads
        .iter()
        .filter(|quad| {
            quad.alpha()
                .is_some_and(|alpha| alpha > 0.0 && alpha <= 0.4)
                && (13.0..15.0).contains(&quad.width())
        })
        .count();
    assert_eq!(early_ring, 2, "one ring around each line's endpoint");

    set_pulse(&harness, cx, Some(0.75));
    let late = draw(&harness, cx);
    let widest = late
        .quads
        .iter()
        .filter(|quad| {
            quad.alpha()
                .is_some_and(|alpha| alpha > 0.0 && alpha < 0.35)
        })
        .map(|quad| quad.width())
        .fold(0.0_f32, f32::max);
    assert!(
        widest > 13.0,
        "the ring expands as the phase advances: {widest}"
    );
}

#[gpui_kit::test]
fn no_pulse_phase_draws_no_ring(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let frame = draw(&harness, cx);

    assert!(
        frame.quads.iter().all(|quad| !quad
            .alpha()
            .is_some_and(|alpha| alpha > 0.0 && alpha < 0.35)),
        "without a phase the endpoint is a plain dot"
    );
}

#[gpui_kit::test]
fn the_hovered_time_sits_in_the_top_strip(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(&harness, cx, Some(plot.center()));
    let frame = draw(&harness, cx);
    let crosshair_x = f32::from(plot.origin.x) + f32::from(plot.size.width) / 2.0;

    let label = frame
        .text("Jan 1, 12:30 AM")
        .expect("the hovered sample's time is drawn");
    assert_close(
        f32::from(label.origin.x),
        crosshair_x + 8.0,
        2.0,
        "the time is left-aligned eight pixels right of the crosshair",
    );
    assert!(
        f32::from(label.origin.y) < f32::from(plot.origin.y),
        "the time is drawn above the plot"
    );
    assert_eq!(f32::from(label.font_size), 12.0);
}

#[gpui_kit::test]
fn a_hover_near_the_right_edge_flips_the_time_label_left(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(
        &harness,
        cx,
        Some(point(plot.right() - px(20.0), plot.center().y)),
    );
    let frame = draw(&harness, cx);
    let crosshair_x = f32::from(plot.origin.x) + f32::from(plot.size.width) * 59.0 / 60.0;

    let label = frame
        .text("Jan 1, 12:59 AM")
        .expect("the hovered sample's time is drawn");
    assert!(
        f32::from(label.origin.x) + f32::from(label.width) <= crosshair_x - 8.0 + 2.0,
        "the time flips to the left of the crosshair when it would overflow"
    );
    assert!(
        f32::from(label.origin.x) >= 0.0,
        "and it stays inside the element"
    );
}

#[gpui_kit::test]
fn a_pointer_only_frame_reuses_the_prepared_chart(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let caches = caches(&harness, cx);
    let plot = draw(&harness, cx).plot();
    let prepared = caches.prepared_builds();
    assert!(prepared >= 1, "a drawn chart has prepared a frame");

    set_pointer(&harness, cx, Some(plot.center()));
    draw(&harness, cx);
    assert_eq!(
        caches.prepared_builds(),
        prepared,
        "a pointer that moved does not rebuild the geometry"
    );

    cx.update(|_, cx| {
        harness.update(cx, |harness, _| {
            harness.snapshot = Arc::new((*harness.snapshot).clone());
        });
    });
    draw(&harness, cx);
    assert_eq!(
        caches.prepared_builds(),
        prepared + 1,
        "a new snapshot does rebuild it"
    );
}

#[gpui_kit::test]
fn the_vertical_grid_starts_below_the_time_strip_and_reaches_the_bottom_band(
    cx: &mut TestAppContext,
) {
    let (harness, cx) = chart_window(cx, snapshot());
    let frame = draw(&harness, cx);
    let plot = frame.plot();
    let grid = cx.update(|_, cx| cx.theme().roles.chart.grid.hsla);
    let dots: Vec<&PaintedQuad> = frame
        .quads
        .iter()
        .filter(|quad| quad.color == Some(grid) && quad.width() <= 1.5 && quad.height() <= 1.5)
        .collect();
    assert!(!dots.is_empty(), "the grid is drawn");

    let top = dots
        .iter()
        .map(|dot| f32::from(dot.bounds.origin.y))
        .fold(f32::INFINITY, f32::min);
    assert!(
        top >= f32::from(plot.origin.y) - 0.01,
        "the vertical grid stops at the plot's top instead of running through the time \
         strip: top {top} vs plot top {}",
        f32::from(plot.origin.y)
    );

    let bottom = dots
        .iter()
        .map(|dot| f32::from(dot.bounds.bottom()))
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        bottom > f32::from(plot.bottom()),
        "the vertical grid still runs through the x-label band: bottom {bottom}"
    );
}

#[gpui_kit::test]
fn a_hover_dot_on_the_left_plot_edge_is_not_cut_by_the_plot_mask(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(
        &harness,
        cx,
        Some(point(plot.origin.x + px(0.5), plot.center().y)),
    );
    let frame = draw(&harness, cx);

    let dot = frame
        .quads
        .iter()
        .find(|quad| {
            (quad.width() - 12.0).abs() <= 0.01 && quad.alpha().is_some_and(|alpha| alpha < 0.2)
        })
        .expect("the hover dot's wash is drawn at the first sample");
    let left = f32::from(dot.bounds.origin.x);
    let plot_left = f32::from(plot.origin.x);

    assert_close(
        left,
        plot_left - 6.0,
        0.01,
        "the wash around a dot centred on the plot's left edge reaches six pixels past it",
    );
    assert!(
        f32::from(dot.mask.origin.x) <= left + 0.01,
        "the dot mask keeps the whole wash visible: mask left {} vs dot left {left}",
        f32::from(dot.mask.origin.x)
    );
}

#[gpui_kit::test]
fn a_hover_dot_on_the_plot_edge_is_not_cut_by_the_plot_mask(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pointer(
        &harness,
        cx,
        Some(point(plot.right() - px(1.0), plot.center().y)),
    );
    let frame = draw(&harness, cx);

    let dot = frame
        .quads
        .iter()
        .find(|quad| {
            (quad.width() - 12.0).abs() <= 0.01 && quad.alpha().is_some_and(|alpha| alpha < 0.2)
        })
        .expect("the hover dot's wash is drawn at the last sample");
    let right = f32::from(dot.bounds.right());
    let plot_right = f32::from(plot.right());

    assert!(
        right > plot_right,
        "the dot is centred on the plot's right edge, so its wash reaches past it: \
         {right} vs {plot_right}"
    );
    assert!(
        f32::from(dot.mask.right()) >= right - 0.01,
        "the dot mask keeps the whole wash visible: mask right {} vs dot right {right}",
        f32::from(dot.mask.right())
    );
    let label = frame.text("10%").expect("the axis labels a tick");
    assert!(
        f32::from(dot.mask.right()) < f32::from(label.origin.x),
        "the dot mask stays left of the y-label column"
    );
}

#[gpui_kit::test]
fn the_endpoint_pulse_reaches_past_the_plot_edge_without_touching_the_labels(
    cx: &mut TestAppContext,
) {
    let (harness, cx) = chart_window(cx, snapshot());
    let plot = draw(&harness, cx).plot();
    set_pulse(&harness, cx, Some(1.9));
    let frame = draw(&harness, cx);

    let ring = frame
        .quads
        .iter()
        .filter(|quad| {
            quad.alpha()
                .is_some_and(|alpha| alpha > 0.0 && alpha < 0.35)
        })
        .max_by(|left, right| left.width().total_cmp(&right.width()))
        .expect("the pulse ring is drawn");
    assert!(
        ring.width() > 16.0,
        "a late phase draws the widest ring: {}",
        ring.width()
    );

    let right = f32::from(ring.bounds.right());
    let plot_right = f32::from(plot.right());
    assert!(
        right > plot_right,
        "the pulse is centred on the plot's right edge, so it reaches past it: \
         {right} vs {plot_right}"
    );
    assert!(
        f32::from(ring.mask.right()) >= right - 0.01,
        "the dot mask keeps the whole pulse visible: mask right {} vs ring right {right}",
        f32::from(ring.mask.right())
    );

    let label = frame.text("30%").expect("the axis labels its top tick");
    assert!(
        right <= f32::from(label.origin.x),
        "the pulse stays left of the y-label column: {right} vs {}",
        f32::from(label.origin.x)
    );
}

#[gpui_kit::test]
fn an_endpoint_pulse_at_the_axis_top_is_not_cut_by_its_mask(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, two_series(100.0, 50.0));
    let plot = draw(&harness, cx).plot();
    set_pulse(&harness, cx, Some(1.9));
    let frame = draw(&harness, cx);

    let ring = frame
        .quads
        .iter()
        .filter(|quad| {
            quad.alpha()
                .is_some_and(|alpha| alpha > 0.0 && alpha < 0.35)
                && f32::from(quad.bounds.origin.y) < f32::from(plot.origin.y)
        })
        .max_by(|left, right| left.width().total_cmp(&right.width()))
        .expect("the pulse ring at the axis top is drawn");
    let top = f32::from(ring.bounds.origin.y);
    let plot_top = f32::from(plot.origin.y);

    assert!(
        top < plot_top - 6.0,
        "the ring reaches past the hover dot's wash at the axis top: {top} vs {plot_top}"
    );
    assert!(
        f32::from(ring.mask.origin.y) <= top + 0.01,
        "the endpoint mask keeps the ring's top visible: mask top {} vs ring top {top}",
        f32::from(ring.mask.origin.y)
    );
}

#[gpui_kit::test]
fn the_y_labels_fit_the_narrowed_gutter(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, two_series(100.0, 50.0));
    let frame = draw(&harness, cx);
    let plot = frame.plot();

    let label = frame
        .text("100%")
        .expect("a chart at certainty labels its hundred-percent tick");
    let right = f32::from(label.origin.x) + f32::from(label.shaped_width);
    assert!(
        right <= f32::from(frame.bounds.right()) + 0.01,
        "the widest y label still fits the gutter: text right {right}, bounds right {}",
        f32::from(frame.bounds.right())
    );
    assert!(
        f32::from(label.origin.x) >= f32::from(plot.right()) + 9.0,
        "the y labels start clear of the dot mask"
    );
}

#[gpui_kit::test]
fn a_line_that_ended_before_the_hover_grows_no_dot_or_label(cx: &mut TestAppContext) {
    let (harness, cx) = chart_window(cx, snapshot_with_an_ended_line());
    let plot = draw(&harness, cx).plot();
    set_pointer(
        &harness,
        cx,
        Some(point(plot.right() - px(1.0), plot.center().y)),
    );
    let frame = draw(&harness, cx);

    assert_eq!(
        frame.label_boxes().len(),
        1,
        "only the line that reaches the hovered sample is named"
    );
    assert!(
        frame.text("No 23.5%").is_none(),
        "the line that stopped halfway grows no label at a later x"
    );
    assert!(
        frame.text("Yes 26.0%").is_some(),
        "the line that still covers the sample keeps its label"
    );

    let washes = frame
        .quads
        .iter()
        .filter(|quad| {
            (quad.width() - 12.0).abs() <= 0.01
                && quad.alpha().is_some_and(|alpha| (alpha - 0.1).abs() < 0.01)
        })
        .count();
    assert_eq!(
        washes, 1,
        "only the line that covers the sample grows a hover dot"
    );
}

//! What a chart does with the pointer.
//!
//! A tooltip is not an element: it is drawn by the same canvas as the series, on
//! the frame that follows the pointer move that asked for it. So what is worth
//! pinning is the path from a move to the hover a frame draws with — the frame the
//! move asks for, the commit that frame performs, and the box it draws in.

use std::{rc::Rc, str::FromStr, sync::Arc};

use gpui_kit::{
    Bounds, Context, IntoElement, Modifiers, ParentElement, Pixels, Render, Size, Styled,
    TestAppContext, VisualTestContext, Window, div, point, px, size,
};
use market_participants_model::{
    decimal::{Ratio, Tick},
    listing::rules::ListingPrecision,
};

use super::*;
use crate::{
    chart::canvas::ChartState,
    model::{LongShortRatioPoint, MetricKind, StatisticsPoints, StatisticsSnapshot},
    service::StatisticsStreamMessage,
};

/// One chart, as a window mounts it.
struct Harness {
    chart: ChartDispatch,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.chart.entity())
    }
}

/// A window of thirty intervals, which is what a chart is normally drawn from.
fn ratio_points() -> Arc<[LongShortRatioPoint]> {
    (0..30)
        .map(|index| LongShortRatioPoint {
            timestamp: 1_700_000_000_000 + i64::from(index) * 300_000,
            ratio: Ratio::from_str(&format!("{}.5", index % 5 + 1))
                .expect("the fixture ratio should parse"),
        })
        .collect::<Vec<_>>()
        .into()
}

fn ratio_window() -> StatisticsSnapshot {
    StatisticsSnapshot {
        revision: 1,
        metadata: Some(ListingPrecision {
            price_tick: Tick::from_inner(100_000_000_000_000_000).expect("a valid tick"),
            quantity_tick: Tick::from_inner(1_000_000_000_000_000).expect("a valid tick"),
        }),
        points: StatisticsPoints::LongShortRatio(ratio_points()),
    }
}

/// Mounts one ratio chart in a window of its own, already showing a window.
///
/// A canvas resolves its layout in prepaint, so a frame has to be drawn before
/// anything is known about where the plot is.
fn chart_window(cx: &mut TestAppContext) -> (Rc<ChartState>, &mut VisualTestContext) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
    });
    let (harness, cx) = cx.add_window_view(|_, cx| {
        let mut chart =
            ChartDispatch::for_metric(cx, "BTC", "USDT", MetricKind::LongShortAccountRatio)
                .expect("a ratio metric should build a ratio chart");
        assert!(
            chart.set_snapshot(&StatisticsStreamMessage::Snapshot(Arc::new(ratio_window()))),
            "the first window is always a change"
        );
        Harness { chart }
    });
    let state = harness.read_with(cx, |harness, _| harness.chart.state());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    (state, cx)
}

/// The box the last frame laid the plot out in.
fn plot(state: &ChartState) -> Bounds<Pixels> {
    let plot = state
        .plot()
        .expect("a laid-out frame records where its plot is");
    assert!(
        plot.size.width > Pixels::ZERO && plot.size.height > Pixels::ZERO,
        "the plot has to have room before a pointer can be over it"
    );
    plot
}

/// Delivers the frame a pointer move asked for.
///
/// The move itself only queues a position: the frame that follows is what applies
/// it, and that frame is the one the next-frame callback requests. A test has no
/// frame loop, so it delivers both halves by hand.
fn draw_the_frame_the_pointer_asked_for(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.refresh();
    });
    cx.run_until_parked();
}

#[gpui_kit::test]
fn a_chart_should_fill_the_box_it_is_given(cx: &mut TestAppContext) {
    let (state, cx) = chart_window(cx);
    let boxes = state.boxes().expect("a laid-out frame records every box");
    let window = cx.update(|window, _| window.bounds());

    assert_eq!(
        boxes.content.size.height, window.size.height,
        "a chart grows into the height it is given rather than keeping its floor"
    );
    assert_eq!(
        boxes.legend.bottom(),
        boxes.content.bottom(),
        "the legend sits on the content's bottom edge, with nothing below it"
    );
    assert_eq!(boxes.content.origin, window.origin);
}

/// The box the frame the chart prepared last draws its content in.
///
/// It is the prepared layout rather than the one a pointer resolves against —
/// [`ChartState::boxes`] — because that one is only rewritten by a frame that reuses
/// an already prepared layout, so it can lag the frame a resize produced. A resize is
/// drawn by the frame that prepares it, and that frame is the one this test is about.
fn drawn_content(state: &ChartState) -> Size<Pixels> {
    state
        .cache
        .borrow()
        .layout
        .expect("a prepared frame records the box it was prepared from")
        .content
        .size
}

#[gpui_kit::test]
fn a_chart_should_follow_a_pane_that_shrank(cx: &mut TestAppContext) {
    let (state, cx) = chart_window(cx);
    let wide = drawn_content(&state);
    assert!(
        wide.width > px(700.0) && wide.height > px(400.0),
        "the fixture window has to be wider than the box it shrinks to: {wide:?}"
    );

    cx.simulate_resize(size(px(700.0), px(400.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert_eq!(
        drawn_content(&state),
        size(px(700.0), px(400.0)),
        "a pane that shrank redraws the chart at the box it now has"
    );
}

#[gpui_kit::test]
fn a_chart_should_follow_a_pane_that_shrank_below_its_own_minimum(cx: &mut TestAppContext) {
    let (state, cx) = chart_window(cx);

    cx.simulate_resize(size(px(360.0), px(240.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert_eq!(
        drawn_content(&state),
        size(px(520.0), px(280.0)),
        "a pane below the chart's floor gets the floor rather than the box it had before"
    );
}

#[gpui_kit::test]
fn a_pointer_over_the_plot_should_hover_the_point_under_it(cx: &mut TestAppContext) {
    let (state, cx) = chart_window(cx);
    let plot = plot(&state);

    cx.simulate_mouse_move(plot.center(), None, Modifiers::default());
    draw_the_frame_the_pointer_asked_for(cx);

    let hover = state
        .hovered()
        .expect("a pointer inside the plot hovers a point");
    assert!(
        plot.contains(&hover.cursor),
        "the hover remembers where the pointer was"
    );
}

#[gpui_kit::test]
fn a_pointer_under_the_plot_should_not_hover_anything(cx: &mut TestAppContext) {
    let (state, cx) = chart_window(cx);
    let plot = plot(&state);

    cx.simulate_mouse_move(
        point(plot.center().x, plot.bottom() + plot.size.height),
        None,
        Modifiers::default(),
    );
    draw_the_frame_the_pointer_asked_for(cx);

    assert!(
        state.hovered().is_none(),
        "a pointer below the plot is not a pointer on a point"
    );
}

#[gpui_kit::test]
fn a_leaving_pointer_should_stop_hovering(cx: &mut TestAppContext) {
    let (state, cx) = chart_window(cx);
    let plot = plot(&state);
    cx.simulate_mouse_move(plot.center(), None, Modifiers::default());
    draw_the_frame_the_pointer_asked_for(cx);
    assert!(state.hovered().is_some());

    cx.simulate_mouse_move(
        point(plot.center().x, plot.bottom() + plot.size.height),
        None,
        Modifiers::default(),
    );
    draw_the_frame_the_pointer_asked_for(cx);

    assert!(
        state.hovered().is_none(),
        "a hover that no longer has a point under it is dropped"
    );
}

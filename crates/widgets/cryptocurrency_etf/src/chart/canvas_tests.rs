//! What a chart does with the pointer, and what it does with the box it is given.
//!
//! A tooltip is not an element: it is drawn by the same canvas as the bars, on the
//! frame that follows the pointer move that asked for it. So what is worth pinning is
//! the path from a move to the hover a frame draws with — the position the move
//! queues, the commit that frame performs, and the box the frame draws in.

use std::rc::Rc;

use gpui_kit::{
    Bounds, Context, IntoElement, Modifiers, ParentElement, Pixels, Render, Size, Styled,
    TestAppContext, VisualTestContext, Window, div, point, px, size,
};
use i18n::language::codes::LanguageCode;

use super::{
    axes::point_x,
    canvas::{ChartState, CryptocurrencyEtfChart},
    test_support::dataset,
};
use crate::model::{CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow};

/// How many days the chart in these tests is drawn from.
const DAYS: usize = 12;

/// One chart, as a window mounts it.
///
/// It renders the chart the way the pane does, commit included: the pane applies the
/// pointer position a move queued before it builds the frame, because two things in
/// that frame are drawn from it — the chart, and the row of readings above it.
struct Harness {
    chart: CryptocurrencyEtfChart,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.chart.sync_locale(cx);
        self.chart.commit_pointer();
        div().size_full().child(self.chart.entity(cx))
    }
}

/// Mounts one chart in a window of its own.
///
/// A canvas resolves its layout in prepaint, so a frame has to be drawn before
/// anything is known about where the plot is.
fn chart_window(
    cx: &mut TestAppContext,
) -> (
    Rc<ChartState>,
    gpui_kit::Entity<Harness>,
    &mut VisualTestContext,
) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
    });
    let (harness, cx) = cx.add_window_view(|_, cx| {
        let chart = CryptocurrencyEtfChart::new(
            cx,
            dataset(&[1_000.0; DAYS]),
            CryptocurrencyEtfFundSelection::All,
            CryptocurrencyEtfWindow::All,
        );
        Harness { chart }
    });
    let state = harness.read_with(cx, |harness, _| harness.chart.state());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    (state, harness, cx)
}

/// The box the last frame laid the plot out in.
fn plot(state: &ChartState) -> Bounds<Pixels> {
    let plot = state
        .boxes()
        .expect("a laid-out frame records where its plot is")
        .plot;
    assert!(
        plot.size.width > Pixels::ZERO && plot.size.height > Pixels::ZERO,
        "the plot has to have room before a pointer can be over it"
    );
    plot
}

/// Delivers the frame a pointer move asked for.
///
/// The move itself only queues a position: the frame that follows is what applies it,
/// and that frame is the one the next-frame callback requests. A test has no frame
/// loop, so it delivers both halves by hand.
fn draw_the_frame_the_pointer_asked_for(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.refresh();
    });
    cx.run_until_parked();
}

#[gpui_kit::test]
fn a_chart_should_fill_the_box_it_is_given(cx: &mut TestAppContext) {
    let (state, _, cx) = chart_window(cx);
    let boxes = state.boxes().expect("a laid-out frame records every box");
    let window = cx.update(|window, _| window.bounds());

    assert_eq!(
        boxes.content.size.height, window.size.height,
        "a chart grows into the height it is given rather than keeping its floor"
    );
    assert_eq!(
        boxes.time_axis.bottom(),
        boxes.content.bottom(),
        "the time axis sits on the content's bottom edge, with nothing below it"
    );
    assert_eq!(boxes.content.origin, window.origin);
    assert_eq!(
        boxes.right_axis.right(),
        boxes.content.right(),
        "and the right axis reaches the content's right edge"
    );
}

/// The box the frame the chart prepared last draws its content in.
///
/// It is the prepared layout rather than the one a pointer resolves against,
/// because those are two different questions and only the first is what a resize
/// has to change. The state a pointer reads is only refreshed by a frame that
/// reuses a prepared frame, so it can lag the frame a resize produced.
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
    let (state, _, cx) = chart_window(cx);
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
    let (state, _, cx) = chart_window(cx);

    cx.simulate_resize(size(px(360.0), px(240.0)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    assert_eq!(
        drawn_content(&state),
        size(px(640.0), px(360.0)),
        "a pane below the chart's floor gets the floor rather than the box it had before"
    );
}

#[gpui_kit::test]
fn a_pointer_over_the_plot_should_hover_the_day_under_it(cx: &mut TestAppContext) {
    let (state, _, cx) = chart_window(cx);
    let plot = plot(&state);

    cx.simulate_mouse_move(plot.center(), None, Modifiers::default());
    draw_the_frame_the_pointer_asked_for(cx);

    let hover = state
        .hovered()
        .expect("a pointer inside the plot hovers a day");
    assert!(hover.index < DAYS);
    assert_eq!(
        hover.cursor_y,
        px(f32::from(plot.center().y).round()),
        "the hover remembers how high the pointer was, to the pixel"
    );
}

#[gpui_kit::test]
fn the_pointer_should_land_on_the_nearest_day(cx: &mut TestAppContext) {
    let (state, _, cx) = chart_window(cx);
    let plot = plot(&state);

    for (index, expected) in [(0, 0), (5, 5), (DAYS - 1, DAYS - 1)] {
        cx.simulate_mouse_move(
            point(point_x(plot, DAYS, index), plot.center().y),
            None,
            Modifiers::default(),
        );
        draw_the_frame_the_pointer_asked_for(cx);

        assert_eq!(
            state.hovered().map(|hover| hover.index),
            Some(expected),
            "a pointer on the {index}th day hovers it"
        );
    }
}

#[gpui_kit::test]
fn a_pointer_off_the_plot_should_not_hover_anything(cx: &mut TestAppContext) {
    let (state, _, cx) = chart_window(cx);
    let plot = plot(&state);

    cx.simulate_mouse_move(
        point(plot.center().x, plot.bottom() + plot.size.height),
        None,
        Modifiers::default(),
    );
    draw_the_frame_the_pointer_asked_for(cx);

    assert!(
        state.hovered().is_none(),
        "a pointer below the plot is not a pointer on a day"
    );
}

#[gpui_kit::test]
fn a_leaving_pointer_should_stop_hovering(cx: &mut TestAppContext) {
    let (state, _, cx) = chart_window(cx);
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
        "a hover that no longer has a day under it is dropped"
    );
}

#[gpui_kit::test]
fn a_move_that_changes_nothing_should_not_ask_for_a_frame(cx: &mut TestAppContext) {
    let (_, harness, cx) = chart_window(cx);
    let plot = plot(&harness.read_with(cx, |harness, _| harness.chart.state()));

    cx.simulate_mouse_move(plot.center(), None, Modifiers::default());
    draw_the_frame_the_pointer_asked_for(cx);
    assert!(
        harness
            .read_with(cx, |harness, _| harness.chart.hover_index())
            .is_some()
    );

    // A move inside the same day, by less than a pixel, is not a change: the chart
    // must not ask for a frame of its own for it.
    cx.simulate_mouse_move(
        point(plot.center().x + px(1.0), plot.center().y + px(0.2)),
        None,
        Modifiers::default(),
    );
    let committed = cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        harness.update(cx, |harness, _| harness.chart.commit_pointer())
    });
    assert!(
        !committed,
        "a pointer that moved within one day and one pixel is not a new reading"
    );

    // A whole pixel higher is.
    cx.simulate_mouse_move(
        point(plot.center().x + px(1.0), plot.center().y + px(1.4)),
        None,
        Modifiers::default(),
    );
    let committed = cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        harness.update(cx, |harness, _| harness.chart.commit_pointer())
    });
    assert!(committed, "a pointer that moved a pixel is a new reading");
}

#[gpui_kit::test]
fn a_language_change_should_rewrite_the_labels_a_chart_is_already_showing(cx: &mut TestAppContext) {
    let (_, harness, cx) = chart_window(cx);
    let english = cx.update(|_, cx| {
        harness
            .read(cx)
            .chart
            .metrics()
            .expect("a window with days has a reading")
    });
    assert_eq!(english.caption.as_ref(), "ETF net inflow");

    cx.update(|window, cx| {
        cx.set_global(LanguageCode::ZhCn);
        window.refresh();
    });
    cx.run_until_parked();

    let chinese = cx.update(|_, cx| {
        harness
            .read(cx)
            .chart
            .metrics()
            .expect("a window with days has a reading")
    });
    assert_eq!(
        chinese.caption.as_ref(),
        "ETF 净流入",
        "an answer arrives every eight hours, so a chart has to be relabelled without one"
    );
    assert_eq!(chinese.date.as_ref(), "2024-01-12");
}

#[gpui_kit::test]
fn the_pointer_should_be_kept_out_of_a_window_it_is_no_longer_over(cx: &mut TestAppContext) {
    let (state, harness, cx) = chart_window(cx);
    let plot = plot(&state);
    cx.simulate_mouse_move(plot.center(), None, Modifiers::default());
    draw_the_frame_the_pointer_asked_for(cx);
    assert!(state.hovered().is_some());

    // The window is replaced by a chart of one day: the hover it was holding has no
    // day under it any more, and a marker for it would be drawn past the end.
    cx.update(|window, cx| {
        harness.update(cx, |harness, cx| {
            harness.chart.set_view(
                cx,
                dataset(&[1_000.0]),
                CryptocurrencyEtfFundSelection::All,
                CryptocurrencyEtfWindow::All,
            );
        });
        window.refresh();
    });
    cx.run_until_parked();

    assert!(
        state.hovered().is_none(),
        "a hover past the end of a replaced window is dropped rather than clamped"
    );
}

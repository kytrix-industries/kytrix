//! Tests for the indicator settings panel.
//!
//! The panel is an entity of its own, drawn inside a window but outside the
//! framework's pane, so what these tests drive is what a user drives: the
//! elements it draws, the fields it holds, and the configuration it writes back
//! through the widget.

use std::time::Duration;

use base_ui::{
    animation::ANIMATION_TICK,
    color_picker::{ColorPicker, ColorPickerEvent},
    icon::IconKind,
    input::{InputState, InputStateEvent},
    size::UnifiedSize,
    style::{palette::Palette, roles::Roles, theme::ThemeExt},
};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, Hsla, IntoElement, Modifiers, ParentElement, Pixels, Quad,
    Render, SharedString, Styled, TestAppContext, VisualTestContext, Window, div, hsla, point, px,
    size, test::TestWindowExt as _,
};

use super::{
    colors::{self, ColorKey},
    inputs, rows,
    view::{IndicatorPanel, PANEL_WIDTH, card_colors},
};
use crate::{
    indicator::{
        INDICATOR_CATALOG, IndicatorId, LineColor, SettingControl, SettingKey, definition_of,
    },
    view::{
        CandlestickWidget,
        tests::{WidgetUnderTest, press, widget},
    },
};

/// The panel as the root of its own window: the framework's body is left out, so
/// the panel's controls are the only things a click can land on.
struct PanelHarness {
    panel: Entity<IndicatorPanel>,
}

impl Render for PanelHarness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The panel takes [`PANEL_WIDTH`] out of the body it is drawn in, so the
        // harness gives it exactly that: a panel laid out in a maximized test
        // display would fit rows production clips, and a row that overflowed the
        // real width would still measure as if it fitted.
        div().w(px(PANEL_WIDTH)).h_full().child(self.panel.clone())
    }
}

/// A chart the panel can edit, and the panel over it.
fn panel_over(cx: &mut TestAppContext) -> (WidgetUnderTest, Entity<IndicatorPanel>) {
    let built = widget(cx);
    let panel = cx.update(|cx| {
        let widget = built.widget.downgrade();
        cx.new(|cx| IndicatorPanel::new(widget, cx))
    });
    (built, panel)
}

/// Draws the panel in a window, which is where its controls are hit-testable.
fn draw<'a>(
    cx: &'a mut TestAppContext,
    panel: &Entity<IndicatorPanel>,
) -> &'a mut VisualTestContext {
    let (_, window) = cx.add_window_view(|_, _| PanelHarness {
        panel: panel.clone(),
    });
    window.run_until_parked();
    window
}

/// Draws the frame the panel asked for, and lets its effects settle.
fn redraw(window: &mut VisualTestContext) {
    window.update(|window, _| window.refresh());
    window.run_until_parked();
}

/// Delivers every frame the window has already asked for, and lets whatever asked
/// for them arrive, so that what a test counts afterwards is what its own change
/// asked for.
///
/// Mounting the panel starts transitions of its own — every card is drawn for the
/// first time, and a transition that is already at its target still has to be
/// settled before the count means anything. Settling a transition means giving it
/// the time it runs for: after each delivered frame the simulated clock is moved
/// on by [`ANIMATION_TICK`], so a transition that is genuinely in flight reaches
/// its target on the next frame instead of asking for frames forever at a clock
/// that never moves. The tick is the longest a transition in this panel runs, so
/// one of them per frame is enough for all of them.
///
/// The loop is bounded by a frame count rather than by time, because a bound that
/// runs out is a failure of its own: a window still asking for frames after
/// twenty ticks of a simulated clock is animating something that never converges,
/// and this helper reports it rather than hanging on it.
fn settle_frames(window: &mut VisualTestContext) {
    for _ in 0..20 {
        if window.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            return;
        }
        window.update(|window, cx| window.render_frame(cx));
        window.executor().advance_clock(ANIMATION_TICK);
    }
    panic!(
        "the window is still asking for frames after twenty {ANIMATION_TICK:?} steps: \
         something it started is not converging on its target"
    );
}

/// The two colours one card's box is painted with, read off the frame it was
/// painted in.
///
/// `debug_bounds` says where the box is, in logical pixels; what it is painted
/// with is the frame's quads, and those are in device pixels after
/// `Window::snap_bounds` has rounded every edge to the device pixel grid. The
/// card's box is therefore matched within half a device pixel — the most that
/// rounding moves an edge — rather than by float equality: at the test window's
/// scale factor and an integral layout the two agree, but a fractional origin
/// would put the quad a fraction of a pixel away from the bounds and an exact
/// comparison would then report "no fill" for a card that is plainly filled.
///
/// A card paints two kinds of quad at its own box: its fill, and — because a
/// border with no background is drawn as four strips around the empty interior —
/// the strips carrying only the border. The fill is the quad there with a
/// background that is not transparent, the border the quad there whose background
/// is transparent and whose border colour is not. Both searches are fatal when
/// nothing is found, so a card that paints no fill at all still fails.
fn painted_card(window: &mut VisualTestContext, card: Bounds<Pixels>) -> (Hsla, Hsla) {
    let (scale, quads) = window.update(|window, _| (window.scale_factor(), window.painted_quads()));
    let card = card.scale(scale);
    // Half a device pixel: the furthest `snap_bounds` can move an edge.
    let at_the_card = |quad: &Quad| {
        [
            quad.bounds.left() - card.left(),
            quad.bounds.top() - card.top(),
            quad.bounds.right() - card.right(),
            quad.bounds.bottom() - card.bottom(),
        ]
        .into_iter()
        .all(|inset| inset.as_f32().abs() <= 0.5)
    };
    let fill = quads
        .iter()
        .filter(|quad| at_the_card(quad))
        .find(|quad| !quad.background.is_transparent())
        .and_then(|quad| quad.background.as_solid())
        .unwrap_or_else(|| {
            panic!(
                "the card paints no solid fill at its box: {card:?} is filled by no quad \
                 within half a device pixel of it (a card with no background at all, or one \
                 painted with something other than a solid colour)"
            )
        });
    let border = quads
        .iter()
        .filter(|quad| at_the_card(quad))
        .find(|quad| quad.background.is_transparent() && !quad.border_color.is_transparent())
        .map(|quad| quad.border_color)
        .unwrap_or_else(|| {
            panic!(
                "the card paints no border at its box: {card:?} has no quad within half a \
                 device pixel of it that carries a border colour around a transparent \
                 background"
            )
        });
    (fill, border)
}

/// Whether two colours are the same to the eye.
///
/// The paint of a settled transition is the value it interpolated to, and the last
/// step of that interpolation is float arithmetic: `a + (b - a) * 1.0` is not
/// `b` for every pair of f32s. The tolerance is far below a colour step (1/1000 of
/// a channel is invisible) and far above that rounding.
fn same_color(left: Hsla, right: Hsla) -> bool {
    (left.h - right.h).abs() < 0.001
        && (left.s - right.s).abs() < 0.001
        && (left.l - right.l).abs() < 0.001
        && (left.a - right.a).abs() < 0.001
}

/// Folds every indicator but `only`.
///
/// The panel opens with all of them expanded, and every expanded line list draws
/// the same test selectors; folding the others is what makes one selector name
/// one list. It is also what lets a test say "this is MA's delete button".
fn focus_on(window: &mut VisualTestContext, panel: &Entity<IndicatorPanel>, only: IndicatorId) {
    fold_all_but(window, panel, &[only]);
}

/// Folds every indicator whose id is not in `kept`.
///
/// The panel opens with all of them expanded, and the expanded ones are tall
/// enough together to push the last cards out of the test window; a test that
/// measures one card against another has to leave only the two it measures.
fn fold_all_but(
    window: &mut VisualTestContext,
    panel: &Entity<IndicatorPanel>,
    kept: &[IndicatorId],
) {
    window.update(|_, cx| {
        for definition in INDICATOR_CATALOG {
            if kept.contains(&definition.id) {
                continue;
            }
            panel.update(cx, |panel, cx| panel.toggle_expanded(definition.id, cx));
        }
    });
    redraw(window);
}

/// The line periods MA's settings hold in the widget's configuration.
fn ma_periods(window: &VisualTestContext, widget: &Entity<CandlestickWidget>) -> Vec<u32> {
    window.read(|cx| {
        let config = widget.read(cx).indicator_config();
        let entry = config
            .entry(IndicatorId::Ma)
            .expect("the catalogue always has MA");
        entry
            .settings
            .lines()
            .expect("MA is a line list")
            .rows
            .iter()
            .map(|line| line.period)
            .collect()
    })
}

/// The colour of MA's last line in the widget's configuration.
fn last_ma_color(window: &VisualTestContext, widget: &Entity<CandlestickWidget>) -> LineColor {
    window.read(|cx| {
        widget
            .read(cx)
            .indicator_config()
            .entry(IndicatorId::Ma)
            .expect("the catalogue always has MA")
            .settings
            .lines()
            .expect("MA is a line list")
            .rows
            .last()
            .expect("the button added a line")
            .color
    })
}

/// Reports one event of a field, the way the field's own state machine does.
fn emit(window: &mut VisualTestContext, input: &Entity<InputState>, event: InputStateEvent) {
    window.update(|_, cx| {
        input.update(cx, |_, cx| cx.emit(event));
    });
    window.run_until_parked();
    redraw(window);
}

#[gpui_kit::test]
fn notifying_the_panel_should_make_it_draw_again(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);

    let drawn = window.read(|cx| panel.read(cx).rendered_frames());
    assert!(drawn >= 1, "the panel draws when it is first put on screen");

    window.update(|_, cx| panel.update(cx, |_, cx| cx.notify()));
    window.run_until_parked();

    assert!(
        window.read(|cx| panel.read(cx).rendered_frames()) > drawn,
        "a notified panel draws another frame"
    );
}

/// Volume has nothing to configure, so it has no caret and cannot be opened: the
/// row exists, the settings do not.
///
/// MA starts expanded, so the caret it draws is the "down" one; a caret is named
/// after the direction it points, which is what lets this test say "volume has no
/// caret at all" rather than "volume has no caret in one of its two states".
#[gpui_kit::test]
fn the_volume_indicator_should_have_no_caret(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);

    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA_DOWN").is_some(),
        "an indicator with settings rows is drawn with a caret"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_CARET_VOL_DOWN").is_none()
            && window.debug_bounds("TEST_PANEL_CARET_VOL_RIGHT").is_none(),
        "volume declares no rows, so there is nothing for a caret to open"
    );
}

/// The last line of a list cannot be removed, and the row it is on does not
/// offer a control that says it could: the delete button is drawn only while the
/// list may still lose a line.
#[gpui_kit::test]
fn the_last_line_of_a_list_should_not_be_removable(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    assert_eq!(
        ma_periods(window, &built.widget),
        vec![5, 10, 20],
        "MA starts with three lines"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_DELETE_LINE_MA_0").is_some(),
        "a list that may lose a line draws that line's delete button"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_DELETE_LINE_MA_1").is_some(),
        "every line of a list above the minimum has a delete button"
    );

    // MA starts with three lines; two removals leave the one the list may not go
    // below.
    for _ in 0..2 {
        window.update(|_, cx| {
            panel.update(cx, |panel, cx| panel.remove_line(IndicatorId::Ma, 0, cx));
        });
    }
    redraw(window);
    assert_eq!(
        ma_periods(window, &built.widget),
        vec![20],
        "two removals leave the last line"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_DELETE_LINE_MA_0").is_none(),
        "the last line is drawn without a delete button, not with a dead one"
    );

    let SettingControl::LineList { min_rows } = definition_of(IndicatorId::Ma).rows[0].control
    else {
        panic!("MA's settings are a line list");
    };
    assert!(
        !rows::can_remove_line(min_rows, 1),
        "one line is the minimum, so it cannot lose another"
    );
    assert!(
        rows::can_remove_line(min_rows, 3),
        "a list of three lines can lose one"
    );

    // The button is not drawn at the minimum, but the configuration refuses the
    // removal as well: an action that slipped past the panel must not leave a
    // list the settings type says cannot be empty.
    window.update(|_, cx| {
        panel.update(cx, |panel, cx| panel.remove_line(IndicatorId::Ma, 0, cx));
    });
    redraw(window);
    assert_eq!(
        ma_periods(window, &built.widget),
        vec![20],
        "the configuration refuses to empty the list"
    );
}

/// The delete button of a line actually deletes that line.
///
/// The list is above its minimum here, so the button exists: pressing it is the
/// only thing in the panel that removes a line, and a button whose handler was
/// dropped would leave the row looking deletable while nothing happened.
#[gpui_kit::test]
fn pressing_a_delete_button_should_remove_its_line(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    assert_eq!(
        ma_periods(window, &built.widget),
        vec![5, 10, 20],
        "MA starts with three lines"
    );
    let reports_before = built.reports();
    let inputs_before = window.read(|cx| panel.read(cx).input_count());
    let pickers_before = window.read(|cx| panel.read(cx).color_picker_count());

    press(window, "TEST_PANEL_DELETE_LINE_MA_1");

    assert_eq!(
        ma_periods(window, &built.widget),
        vec![5, 20],
        "the pressed button removes the line it sits on, and only that line"
    );
    assert_eq!(
        built.reports(),
        reports_before + 1,
        "removing a line is written back exactly once"
    );
    assert_eq!(
        window.read(|cx| panel.read(cx).input_count()),
        inputs_before - 1,
        "the removed line's field is dropped with it"
    );
    assert_eq!(
        window.read(|cx| panel.read(cx).color_picker_count()),
        pickers_before - 1,
        "the removed line's picker is dropped with it"
    );
}

/// The field of a line row sits at the row's left and its controls at the row's
/// right: a row whose children simply follow each other puts the picker and the
/// delete button in the middle of the panel instead of at its edge.
#[gpui_kit::test]
fn a_line_row_should_put_its_field_on_the_left_and_its_controls_on_the_right(
    cx: &mut TestAppContext,
) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let row = window
        .debug_bounds("TEST_PANEL_LINE_ROW_MA_1")
        .expect("MA's second line is drawn while MA is open");
    let field = window
        .debug_bounds("TEST_PANEL_PERIOD_FIELD_MA_1")
        .expect("the line has a period field");
    let picker = window
        .debug_bounds("TEST_PANEL_COLOR_MA_1")
        .expect("the line has a colour picker");
    let delete = window
        .debug_bounds("TEST_PANEL_DELETE_LINE_MA_1")
        .expect("a list of three lines may lose one");

    assert!(
        (field.origin.x - (row.origin.x + px(rows::SETTING_INSET)))
            .as_f32()
            .abs()
            <= 1.0,
        "the field starts at the row's left edge plus the settings inset: {field:?} vs {row:?}"
    );
    assert!(
        (delete.right() - row.right()).as_f32().abs() <= 1.0,
        "the delete button ends at the row's right edge: {delete:?} vs {row:?}"
    );
    assert!(
        picker.right() <= delete.left(),
        "the picker sits left of the delete button: {picker:?} vs {delete:?}"
    );
}

/// A line row fits the panel the design fixes: the field keeps its width, the
/// picker keeps its own, and the row's last control still ends inside the panel.
///
/// The harness draws the panel at [`PANEL_WIDTH`], which is what production gives
/// it, so a field wide enough to overflow the real panel is caught here rather
/// than by a row that only fits the test display. The row's own box would not
/// catch it: the row is as wide as the panel whatever its children do, so what is
/// measured is the right edge of the last control on it.
#[gpui_kit::test]
fn a_line_row_should_fit_the_panel(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let panel_bounds = window
        .debug_bounds("TEST_CANDLESTICK_INDICATOR_PANEL")
        .expect("the panel is drawn");
    let field = window
        .debug_bounds("TEST_PANEL_PERIOD_FIELD_MA_1")
        .expect("the line has a period field");
    let picker = window
        .debug_bounds("TEST_PANEL_COLOR_MA_1")
        .expect("the line has a colour picker");
    let delete = window
        .debug_bounds("TEST_PANEL_DELETE_LINE_MA_1")
        .expect("a list of three lines may lose one");

    assert_eq!(
        panel_bounds.size.width,
        px(PANEL_WIDTH),
        "the harness draws the panel at the width production gives it"
    );
    assert!(
        (field.size.width - px(rows::PERIOD_FIELD_WIDTH))
            .as_f32()
            .abs()
            <= 1.0,
        "the field keeps its width at {PANEL_WIDTH}px: {field:?}"
    );
    assert!(
        (picker.size.width - px(20.0)).as_f32().abs() <= 1.0,
        "the picker keeps its 20px square at {PANEL_WIDTH}px: {picker:?}"
    );
    assert!(
        delete.right() <= panel_bounds.right(),
        "the row's last control ends inside the panel: {delete:?} vs {panel_bounds:?}"
    );
}

/// A list at its minimum draws no delete button, so the picker is the row's last
/// control and has to take its right edge: without the spacer the picker would
/// stop short of it and leave a gap where the button used to be.
#[gpui_kit::test]
fn a_single_line_should_put_its_picker_at_the_rows_right_edge(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    // MA starts with three lines; two removals leave the one the list may not go
    // below.
    for _ in 0..2 {
        window.update(|_, cx| {
            panel.update(cx, |panel, cx| panel.remove_line(IndicatorId::Ma, 0, cx));
        });
    }
    redraw(window);

    assert!(
        window.debug_bounds("TEST_PANEL_DELETE_LINE_MA_0").is_none(),
        "the last line of a list has no delete button"
    );
    let row = window
        .debug_bounds("TEST_PANEL_LINE_ROW_MA_0")
        .expect("the last line is drawn");
    let picker = window
        .debug_bounds("TEST_PANEL_COLOR_MA_0")
        .expect("the last line keeps its colour picker");

    assert!(
        (picker.right() - row.right()).as_f32().abs() <= 1.0,
        "the picker takes the row's right edge once the delete button is gone: \
         {picker:?} vs {row:?}"
    );
}

/// The "+" caption is centred in its button: the small size's 4px horizontal
/// padding would otherwise leave it at the left edge, where it reads like the
/// label of the field above it rather than as the action that adds a line.
#[gpui_kit::test]
fn the_add_line_caption_should_be_centred(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let button = window
        .debug_bounds("TEST_PANEL_ADD_LINE_BUTTON")
        .expect("the button that adds a line is drawn");
    let caption = window
        .debug_bounds("TEST_PANEL_ADD_LINE_LABEL")
        .expect("the button draws its caption");

    assert!(
        (caption.center().x - button.center().x).as_f32().abs() <= 1.0,
        "the caption is centred in the button: {caption:?} vs {button:?}"
    );
    assert!(
        caption.size.width < button.size.width / 2.0,
        "the caption is the width of its text, not of the button: a caption that \
         filled the button would be centred too, and would not be a caption: \
         {caption:?} vs {button:?}"
    );
}

/// A folded indicator's caret points right, the direction its settings open in;
/// pointing left says "back", which is not what pressing the row does.
#[test]
fn a_folded_caret_should_point_right() {
    assert_eq!(
        rows::caret_icon(true),
        IconKind::CaretDown,
        "an open indicator's caret points down, at the settings under it"
    );
    assert_eq!(
        rows::caret_icon(false),
        IconKind::CaretRight,
        "a folded indicator's caret points right, the direction it opens in"
    );
}

/// The caret the panel draws follows the state of the indicator it belongs to.
///
/// The helper's answer is only half of it: a row that ignored it, or passed it
/// the wrong flag, would still draw a caret — just the wrong one. The selector is
/// named after the direction the row actually drew, so the two states can be told
/// apart by what is on screen.
#[gpui_kit::test]
fn folding_an_indicator_should_draw_a_right_caret(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA_DOWN").is_some(),
        "MA starts open, so its caret points at the settings under it"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA_RIGHT").is_none(),
        "an open indicator draws one caret, not both"
    );

    window.update(|_, cx| {
        panel.update(cx, |panel, cx| panel.toggle_expanded(IndicatorId::Ma, cx));
    });
    redraw(window);
    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA_RIGHT").is_some(),
        "a folded indicator's caret points right, the direction its settings open in"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA_DOWN").is_none(),
        "the open caret is gone once the settings are folded away"
    );

    window.update(|_, cx| {
        panel.update(cx, |panel, cx| panel.toggle_expanded(IndicatorId::Ma, cx));
    });
    redraw(window);
    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA_DOWN").is_some(),
        "opening the settings again turns the caret back down"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA_RIGHT").is_none(),
        "and the folded caret is gone with it"
    );
}

/// The indicator title and the parameter labels are `xs`, one size smaller than
/// the section headings they sit under.
///
/// Measured against a heading the same frame drew rather than against a number:
/// the captions are the same kind of text, so the difference between their boxes
/// is the difference between the two sizes, and nothing has to hardcode how tall
/// a line of either one is.
#[gpui_kit::test]
fn the_captions_should_be_smaller_than_a_section_heading(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Macd);

    let heading = window
        .debug_bounds("TEST_PANEL_SECTION_TITLE_SUB")
        .expect("the sub chart's heading is drawn");
    let title = window
        .debug_bounds("TEST_PANEL_NODE_TITLE_MACD")
        .expect("MACD's node row draws its title");
    // MACD's first row is a period row, and its label sits beside the field.
    let label = window
        .debug_bounds("TEST_PANEL_PERIOD_LABEL_MACD_0")
        .expect("MACD's first period row draws its label");

    assert!(
        title.size.height < heading.size.height,
        "the indicator title is smaller than a section heading: {title:?} vs {heading:?}"
    );
    assert!(
        label.size.height < heading.size.height,
        "a parameter label is smaller than a section heading: {label:?} vs {heading:?}"
    );
    assert!(
        window
            .debug_bounds("TEST_PANEL_SECTION_TITLE_MAIN")
            .is_some(),
        "the main chart has a heading of its own, named after its pane"
    );
}

/// The "+" button adds exactly one line, and stays under the list it grows.
#[gpui_kit::test]
fn adding_a_line_should_grow_the_list_below_the_last_one(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let before = ma_periods(window, &built.widget);
    let before_reports = built.reports();
    let pickers_before = window.read(|cx| panel.read(cx).color_picker_count());
    press(window, "TEST_PANEL_ADD_LINE");
    let after = ma_periods(window, &built.widget);

    assert_eq!(after.len(), before.len() + 1, "the + button adds one line");
    assert_eq!(
        &after[..before.len()],
        before.as_slice(),
        "the lines that were already there keep their order and their values"
    );
    assert_eq!(
        built.reports(),
        before_reports + 1,
        "adding a line is written back exactly once"
    );

    // The added line is a new colour field: it gets a picker of its own on its
    // row, and that picker shows the palette colour the line was given.
    assert_eq!(
        window.read(|cx| panel.read(cx).color_picker_count()),
        pickers_before + 1,
        "the new line gets a picker of its own"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_COLOR_MA_3").is_some(),
        "the new line's picker is drawn on its row"
    );
    let added_picker = picker_of(window, &panel, ma_line_key(3));
    assert_eq!(
        window.read(|cx| added_picker.read(cx).color()),
        Hsla::from(last_ma_color(window, &built.widget)),
        "the picker shows the colour the new line was given"
    );

    // The palette is read from the theme the harness installed, not hardcoded:
    // MA had three rows, so the fourth takes the palette's index 3.
    let expected_color =
        window.read(|cx| LineColor::from(cx.theme().components.chart.series[3].hsla));
    let added_color = last_ma_color(window, &built.widget);
    assert_eq!(
        added_color, expected_color,
        "a new line snapshots the next colour of the current appearance's palette"
    );

    // MA now has four rows, so the next addition takes index 4: the palette is
    // the six distinct theme colours, not the first one repeated.
    press(window, "TEST_PANEL_ADD_LINE");
    let expected_next =
        window.read(|cx| LineColor::from(cx.theme().components.chart.series[4].hsla));
    let added_next = last_ma_color(window, &built.widget);
    assert_eq!(
        added_next, expected_next,
        "each addition advances through the palette, not to one fixed colour"
    );

    let list = window
        .debug_bounds("TEST_PANEL_LINE_LIST")
        .expect("the list is drawn while its indicator is open");
    let add = window
        .debug_bounds("TEST_PANEL_ADD_LINE")
        .expect("the button that adds a line is drawn");
    assert!(
        add.top() >= list.bottom(),
        "the + button sits under the last line, not among them: {add:?} and {list:?}"
    );
}

/// A period the field cannot read neither reaches the configuration nor hides
/// what is wrong with it; leaving the field puts the stored value back.
#[gpui_kit::test]
fn a_text_that_is_not_a_period_should_not_be_written_back(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let input = window
        .read(|cx| {
            panel
                .read(cx)
                .input_entity(IndicatorId::Ma, Some(0), SettingKey::Lines)
        })
        .expect("the first line of MA has a field");
    emit(
        window,
        &input,
        InputStateEvent::Change(SharedString::new_static("0")),
    );

    assert_eq!(
        ma_periods(window, &built.widget),
        vec![5, 10, 20],
        "a period the field does not accept is not written back"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_INVALID_PERIOD").is_some(),
        "the row says what a period has to be"
    );

    emit(
        window,
        &input,
        InputStateEvent::Blur(SharedString::new_static("0")),
    );

    assert_eq!(
        ma_periods(window, &built.widget),
        vec![5, 10, 20],
        "leaving the field with the same text writes nothing either"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_INVALID_PERIOD").is_none(),
        "the frame goes away when the stored value is put back"
    );
    assert_eq!(
        window.read(|cx| input.read(cx).value(cx).to_string()),
        "5",
        "the field shows the value the configuration holds"
    );
}

/// A period the field can read is written back when the caret leaves it, and
/// reported once: typing alone changes nothing.
#[gpui_kit::test]
fn a_valid_period_should_be_written_back_when_the_field_is_left(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let input = window
        .read(|cx| {
            panel
                .read(cx)
                .input_entity(IndicatorId::Ma, Some(0), SettingKey::Lines)
        })
        .expect("the first line of MA has a field");
    let before = built.reports();
    emit(
        window,
        &input,
        InputStateEvent::Change(SharedString::new_static("7")),
    );

    assert_eq!(
        ma_periods(window, &built.widget),
        vec![5, 10, 20],
        "typing a period alone does not write it"
    );
    assert_eq!(built.reports(), before, "typing is not a change to publish");

    emit(
        window,
        &input,
        InputStateEvent::Blur(SharedString::new_static("7")),
    );

    assert_eq!(
        ma_periods(window, &built.widget),
        vec![7, 10, 20],
        "leaving a field with a period writes it back"
    );
    assert_eq!(
        built.reports(),
        before + 1,
        "the commit is reported exactly once"
    );
}

/// A removed line takes its field with it: the table is keyed by line, so a line
/// that is gone is a field that is gone.
#[gpui_kit::test]
fn removing_a_line_should_drop_its_field(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let before = window.read(|cx| panel.read(cx).input_count());
    assert!(
        window
            .read(|cx| panel
                .read(cx)
                .input_entity(IndicatorId::Ma, Some(2), SettingKey::Lines))
            .is_some(),
        "the third line of MA has a field"
    );

    window.update(|_, cx| {
        panel.update(cx, |panel, cx| panel.remove_line(IndicatorId::Ma, 2, cx));
    });
    redraw(window);

    assert_eq!(
        window.read(|cx| panel.read(cx).input_count()),
        before - 1,
        "the removed line's field is dropped with it"
    );
    assert!(
        window
            .read(|cx| panel
                .read(cx)
                .input_entity(IndicatorId::Ma, Some(2), SettingKey::Lines))
            .is_none(),
        "the third line is no longer editable"
    );
}

/// One field per editable period, keyed by what it edits: the panel's table is
/// only correct if no two fields claim the same period.
#[gpui_kit::test]
fn every_editable_period_should_have_exactly_one_field(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);

    let config = window
        .read(|cx| panel.read(cx).indicator_config(cx))
        .expect("the widget still holds its configuration");
    let fields = inputs::input_fields(&config);
    let keys: Vec<super::inputs::InputKey> = fields
        .iter()
        .map(|field| super::inputs::InputKey {
            id: field.id,
            row: field.row,
            key: field.key,
        })
        .collect();
    for (index, key) in keys.iter().enumerate() {
        assert!(
            !keys[..index].contains(key),
            "{key:?} is drawn twice, so two fields would edit one period"
        );
    }
    assert_eq!(
        window.read(|cx| panel.read(cx).input_count()),
        fields.len(),
        "every field the configuration has is an entity the panel holds"
    );
}

/// The colour field of MA's line `index`.
fn ma_line_key(index: usize) -> ColorKey {
    ColorKey {
        id: IndicatorId::Ma,
        row: Some(index),
        key: SettingKey::Lines,
    }
}

/// The colour MA's line `index` holds in the widget's configuration.
fn ma_line_color(
    window: &VisualTestContext,
    widget: &Entity<CandlestickWidget>,
    index: usize,
) -> LineColor {
    window.read(|cx| {
        widget
            .read(cx)
            .indicator_config()
            .entry(IndicatorId::Ma)
            .expect("the catalogue always has MA")
            .settings
            .lines()
            .expect("MA is a line list")
            .rows[index]
            .color
    })
}

/// The picker of one colour field as the panel currently holds it.
fn picker_of(
    window: &VisualTestContext,
    panel: &Entity<IndicatorPanel>,
    key: ColorKey,
) -> Entity<ColorPicker> {
    window
        .read(|cx| panel.read(cx).color_picker_entity(key))
        .unwrap_or_else(|| panic!("{key:?} has a picker"))
}

/// Reports one event of a picker, the way the picker's own interactions do.
fn emit_picker(
    window: &mut VisualTestContext,
    picker: &Entity<ColorPicker>,
    event: ColorPickerEvent,
) {
    window.update(|_, cx| {
        picker.update(cx, |_, cx| cx.emit(event));
    });
    window.run_until_parked();
    redraw(window);
}

/// Every colour setting has exactly one picker, keyed by what it edits: two
/// pickers for one colour would be two controls editing one value.
#[gpui_kit::test]
fn every_colour_setting_should_have_exactly_one_picker(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);

    let config = window
        .read(|cx| panel.read(cx).indicator_config(cx))
        .expect("the widget still holds its configuration");
    let fields = colors::color_fields(&config);
    assert_eq!(
        fields.len(),
        12,
        "three MA lines, one EMA line, three RSI lines, MACD's two and KDJ's three"
    );
    let keys: Vec<ColorKey> = fields.iter().map(|field| field.key()).collect();
    for (index, key) in keys.iter().enumerate() {
        assert!(
            !keys[..index].contains(key),
            "{key:?} is drawn twice, so two pickers would edit one colour"
        );
    }
    assert_eq!(
        window.read(|cx| panel.read(cx).color_picker_count()),
        fields.len(),
        "every colour field is an entity the panel holds"
    );
    assert_eq!(
        window
            .debug_bounds("TEST_PANEL_COLOR_MA_0")
            .map(|bounds| bounds.size),
        Some(size(px(20.), px(20.))),
        "the picker is UnifiedSize::Small, the 20px square"
    );
}

/// Every picker shows the colour the configuration stores.
#[gpui_kit::test]
fn every_picker_should_show_the_stored_colour(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);

    let config = window
        .read(|cx| panel.read(cx).indicator_config(cx))
        .expect("the widget still holds its configuration");
    for field in colors::color_fields(&config) {
        let picker = picker_of(window, &panel, field.key());
        assert_eq!(
            window.read(|cx| picker.read(cx).color()),
            Hsla::from(field.color),
            "{:?} shows the colour the configuration holds",
            field.key()
        );
    }
}

/// A picker's report is a confirmed colour: it is stored, and published once.
#[gpui_kit::test]
fn a_colour_commit_should_publish_exactly_once(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    let key = ma_line_key(1);
    let picker = picker_of(window, &panel, key);
    let before = built.reports();
    let committed = hsla(0.7, 0.3, 0.5, 0.9);

    emit_picker(window, &picker, ColorPickerEvent::ColorChanged(committed));

    assert_eq!(
        ma_line_color(window, &built.widget, 1),
        LineColor::from(committed),
        "the confirmed colour is the one the configuration stores"
    );
    assert_eq!(
        built.reports(),
        before + 1,
        "one interaction publishes once"
    );

    let standalone = hsla(0.15, 0.25, 0.35, 1.0);
    emit_picker(window, &picker, ColorPickerEvent::ColorChanged(standalone));
    assert_eq!(
        ma_line_color(window, &built.widget, 1),
        LineColor::from(standalone)
    );
    assert_eq!(
        window.read(|cx| picker.read(cx).color()),
        standalone,
        "the picker is written back to the stored colour when the config moves"
    );
    assert_eq!(
        built.reports(),
        before + 2,
        "a second interaction is a second write"
    );
}

/// A switch row's picker writes the colour field that row controls, not a line.
#[gpui_kit::test]
fn a_switch_picker_should_write_the_colour_of_its_field(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    let changed = hsla(0.12, 0.34, 0.56, 0.78);

    for (id, key) in [
        (IndicatorId::Macd, SettingKey::DifColor),
        (IndicatorId::Kdj, SettingKey::JColor),
    ] {
        let color_key = ColorKey { id, row: None, key };
        let picker = picker_of(window, &panel, color_key);
        emit_picker(window, &picker, ColorPickerEvent::ColorChanged(changed));

        let stored = window.read(|cx| {
            built
                .widget
                .read(cx)
                .indicator_config()
                .entry(id)
                .expect("the catalogue always has the entry")
                .settings
                .color(key)
        });
        assert_eq!(
            stored,
            Some(LineColor::from(changed)),
            "{id:?}::{key:?} is the field the picker writes"
        );
    }
}

/// A switch row draws its picker inside the row, at the row's right edge, and the
/// picker opens the palette when pressed.
#[gpui_kit::test]
fn a_macd_switch_row_should_draw_its_picker_on_the_right(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Macd);

    let row = window
        .debug_bounds("TEST_PANEL_SWITCH_ROW_MACD_DIF")
        .expect("the DIF switch row is drawn while MACD is open");
    let picker = window
        .debug_bounds("TEST_PANEL_COLOR_MACD_DIF")
        .expect("the DIF row draws its colour picker");
    assert!(
        (picker.right() - row.right()).as_f32().abs() <= 1.0,
        "the picker sits at the row's right edge: {picker:?} vs {row:?}"
    );

    press(window, "TEST_PANEL_COLOR_MACD_DIF");
    assert!(
        window.debug_bounds("TEST_COLOR_PICKER_GRID").is_some(),
        "pressing the row's picker opens its palette"
    );
}

/// KDJ's J row offers its own picker, named after the row it controls.
#[gpui_kit::test]
fn a_kdj_switch_row_should_offer_its_picker(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Kdj);

    assert!(
        window.debug_bounds("TEST_PANEL_SWITCH_ROW_KDJ_J").is_some(),
        "the J switch row is drawn while KDJ is open"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_COLOR_KDJ_J").is_some(),
        "the J row draws the picker for J's colour"
    );

    press(window, "TEST_PANEL_COLOR_KDJ_J");
    assert!(
        window.debug_bounds("TEST_COLOR_PICKER_GRID").is_some(),
        "pressing J's picker opens its palette"
    );
}

/// A removed line takes its picker with it, like its input.
#[gpui_kit::test]
fn removing_a_line_should_drop_its_picker(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    let key = ma_line_key(2);
    let before = window.read(|cx| panel.read(cx).color_picker_count());
    assert!(
        window
            .read(|cx| panel.read(cx).color_picker_entity(key))
            .is_some(),
        "the third line of MA has a picker"
    );

    window.update(|_, cx| {
        panel.update(cx, |panel, cx| panel.remove_line(IndicatorId::Ma, 2, cx));
    });
    redraw(window);

    assert_eq!(
        window.read(|cx| panel.read(cx).color_picker_count()),
        before - 1,
        "the removed line's picker is dropped with it"
    );
    assert!(
        window
            .read(|cx| panel.read(cx).color_picker_entity(key))
            .is_none(),
        "the third line is no longer colourable"
    );
}

/// A picker drawn again is the same entity: rebuilding it would lose the tab the
/// user left it on and the caret in its hex field.
#[gpui_kit::test]
fn a_second_frame_should_reuse_the_same_picker(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    let key = ma_line_key(0);

    let first = picker_of(window, &panel, key).entity_id();
    redraw(window);
    let second = picker_of(window, &panel, key).entity_id();

    assert_eq!(first, second, "a redrawn panel keeps its picker entity");
}

/// Choosing a palette cell only edits the picker's panel; the cell's colour reaches
/// the configuration when the user confirms it.
///
/// The popup is drawn as a deferred overlay, so the cell is a window-level
/// element rather than a child of the row: the click is aimed at the grid's first
/// cell, which is where the palette starts.
#[gpui_kit::test]
fn choosing_a_palette_cell_should_publish_the_line_colour_once_it_is_confirmed(
    cx: &mut TestAppContext,
) {
    let (built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    let before = built.reports();
    let before_color = ma_line_color(window, &built.widget, 0);

    press(window, "TEST_PANEL_COLOR_MA_0");
    let grid = window
        .debug_bounds("TEST_COLOR_PICKER_GRID")
        .expect("the picker trigger opens the palette tab");
    window.simulate_click(
        point(grid.origin.x + px(10.0), grid.origin.y + px(10.0)),
        Modifiers::none(),
    );
    window.run_until_parked();
    redraw(window);

    assert_eq!(
        ma_line_color(window, &built.widget, 0),
        before_color,
        "choosing a cell without confirming leaves the stored colour alone"
    );
    assert_eq!(
        built.reports(),
        before,
        "an unconfirmed choice publishes nothing"
    );

    press(window, "TEST_COLOR_PICKER_CONFIRM");
    redraw(window);

    assert_ne!(
        ma_line_color(window, &built.widget, 0),
        before_color,
        "the cell the user confirmed is the line's new colour"
    );
    assert_eq!(
        built.reports(),
        before + 1,
        "confirming a colour publishes it once"
    );
}

/// A folded card is a box around its indicator's node row: it starts above the
/// title and ends below it, and its own padding holds the row away from its edges.
///
/// Three of the four edges are measured, and the padding is what makes the third
/// and fourth assertions different from the second. A card that wrapped nothing
/// would have the title outside it; a card that wrapped only the checkbox would
/// start below the caret; and a card with no padding would hold the row flush
/// against its inset edge, which is what the caret's own corner states — the caret
/// is the node row's first child, so it sits exactly one border plus one padding
/// inside the card's corner.
#[gpui_kit::test]
fn a_folded_card_should_wrap_its_node_row(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    // MA itself is folded too: the caret this measures is the right one a folded
    // card draws, and an expanded MA would draw the down one.
    fold_all_but(window, &panel, &[]);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("a folded MA is drawn as its card");
    let title = window
        .debug_bounds("TEST_PANEL_NODE_TITLE_MA")
        .expect("the card draws MA's node row");
    let caret = window
        .debug_bounds("TEST_PANEL_CARET_MA_RIGHT")
        .expect("a folded indicator draws a right caret");

    assert!(
        card.top() < title.top() && card.bottom() > title.bottom(),
        "the card is the box around the node row, not a line beside it: {card:?} vs {title:?}"
    );
    assert!(
        card.left() < title.left(),
        "the card's padding places its left edge outside the title it holds: {card:?} vs {title:?}"
    );
    // The caret is the node row's first child, so the inset between the card's
    // corner and the caret's is the card's 1px border plus its 4px padding.
    assert!(
        (caret.left() - card.left()).as_f32() > 4.0,
        "the card's padding holds its content away from its edge: {caret:?} vs {card:?}"
    );
    assert!(
        (caret.top() - card.top()).as_f32() > 4.0,
        "and the same padding applies on top, so the row is not flush with the border: \
         {caret:?} vs {card:?}"
    );
}

/// An expanded card holds everything its indicator draws, the "+" button last,
/// and its padding holds that last row away from its own edges.
///
/// The bottom is what tells "the card wraps the indicator" from "the card wraps
/// the title": a card that stopped at the node row would end above the button and
/// above the next indicator's card, and both of those would still leave the
/// title inside *a* card.
///
/// The "+" row is the card's last child and spans its width, so its bottom and
/// right edges are the card's inner edges: the gap between the two boxes is the
/// card's 1px border plus its 4px padding, on the bottom and on the right. Those
/// are the two insets the folded-card test above cannot reach — its only child
/// takes its width from its own content and its height from the row it draws — so
/// padding that went missing on the right or the bottom would leave every other
/// assertion in this file green.
#[gpui_kit::test]
fn an_expanded_card_should_wrap_the_settings_and_the_add_button(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    // Only MA is left open, so the "+" button the selector finds is MA's: every
    // expanded line list draws one under the same selector, and the last one drawn
    // would otherwise be the one measured. EMA stays on screen as a folded card,
    // which is all the comparison with the next card needs.
    focus_on(window, &panel, IndicatorId::Ma);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("an expanded MA is drawn as its card");
    let add = window
        .debug_bounds("TEST_PANEL_ADD_LINE_BUTTON")
        .expect("MA is a line list, so its card draws the button that adds a line");
    let next = window
        .debug_bounds("TEST_PANEL_CARD_EMA")
        .expect("EMA is the card after MA");

    assert!(
        card.bottom() >= add.bottom(),
        "the card reaches past the last thing the indicator draws, the + button: \
         {card:?} vs {add:?}"
    );
    assert!(
        card.bottom() < next.top(),
        "the card ends before the next indicator's card begins, so the settings \
         are MA's and not drawn between two cards: {card:?} vs {next:?}"
    );

    // The "+" row's wrapper, not the button inside it: the button is inset by its
    // own frame, while the wrapper is what the card lays out and is therefore flush
    // with the card's inner edges.
    let row = window
        .debug_bounds("TEST_PANEL_ADD_LINE")
        .expect("the + row is drawn with the button");
    let right_inset = (card.right() - row.right()).as_f32();
    let bottom_inset = (card.bottom() - row.bottom()).as_f32();
    assert!(
        (right_inset - 5.0).abs() < 1.0,
        "the card's right padding is 4px plus its 1px border, so the row it holds \
         stops 5px short of its right edge: {row:?} vs {card:?}"
    );
    assert!(
        (bottom_inset - 5.0).abs() < 1.0,
        "and the same padding is under the row, so the card's last row is not flush \
         with its bottom edge: {row:?} vs {card:?}"
    );
}

/// Two cards are separated by the row gap, and the row gap is 4px.
///
/// The number is stated rather than read from a constant because it is the thing
/// this change fixes: the panel used to be a flat list of rows with a 2px gap, so
/// a change that left the gap alone would still draw a plausible column of cards
/// and has to fail here. The tolerance is half a pixel, which is what a 4px gap
/// needs to survive snapping to the device pixel grid; every other value the code
/// could plausibly carry is more than that away — 2px was the old flat list's, and
/// 5px, 6px and 8px are all further from 4 than half a pixel.
#[gpui_kit::test]
fn cards_should_be_separated_by_the_row_gap(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    fold_all_but(window, &panel, &[IndicatorId::Ma, IndicatorId::Ema]);

    let ma = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("MA's card is drawn");
    let ema = window
        .debug_bounds("TEST_PANEL_CARD_EMA")
        .expect("EMA's card is drawn");

    let gap = (ema.top() - ma.bottom()).as_f32();
    assert!(
        (gap - 4.0).abs() < 0.5,
        "two adjacent cards are one row gap apart, and the gap is 4px: a 2px gap \
         was the old flat list's and must be gone (gap {gap}px, {ma:?} vs {ema:?})"
    );
}

/// A card at rest is the sunken surface with a divider for an edge.
///
/// The recipe is pinned against the theme rather than against a literal, so a
/// theme that moves its sunken surface moves the card with it: a card painted in
/// a value copied from one appearance would pass here in the appearance it was
/// copied from and be wrong in the other.
#[test]
fn a_card_should_be_painted_in_the_sunken_surface() {
    for palette in [Palette::light(), Palette::dark()] {
        let roles = Roles::resolve(&palette);
        let (background, border) = card_colors(&roles, true, false);

        assert_eq!(
            background, roles.surface.sunken.hsla,
            "a resting card is the surface a widget's content sits on"
        );
        assert_eq!(
            border, roles.line.divider.hsla,
            "a resting card's edge is the non-interactive divider, not a control's border"
        );

        let (background, border) = card_colors(&roles, false, false);
        assert_eq!(
            background, roles.surface.sunken.hsla,
            "an expanded card is the same surface as a folded one: opening it must \
             not make it look like a different kind of box"
        );
        assert_eq!(
            border, roles.line.divider.hsla,
            "and it has the same edge; only the pointer makes either move"
        );
    }
}

/// Only a folded card answers the pointer, and what it answers with is a veil over
/// its resting surface and the stronger line for its edge.
///
/// The third state is the one that is easy to get wrong: an expanded card with a
/// hover of its own looks exactly like a folded one to this function, and the only
/// way to tell them apart is to ask what a hovered expanded card returns. It has
/// to return the resting pair.
#[test]
fn only_a_folded_card_should_answer_the_pointer() {
    for palette in [Palette::light(), Palette::dark()] {
        let roles = Roles::resolve(&palette);

        assert_ne!(
            card_colors(&roles, true, true),
            card_colors(&roles, true, false),
            "a pointer on a folded card changes both of its colours"
        );
        let (hovered_background, hovered_border) = card_colors(&roles, true, true);
        assert_ne!(
            hovered_background, roles.surface.sunken.hsla,
            "the hover is a wash laid over the resting surface, not the surface itself"
        );
        assert_eq!(
            hovered_background,
            roles.surface.sunken.hsla.blend(roles.veil.hover.hsla),
            "the wash is the theme's own veil, composited over the resting surface"
        );
        assert_eq!(
            hovered_border, roles.line.strong.hsla,
            "the edge lifts to the role the pointer is drawn with everywhere else"
        );

        assert_eq!(
            card_colors(&roles, false, true),
            card_colors(&roles, false, false),
            "an expanded card draws no hover at all, so the pointer cannot change it"
        );
    }
}

/// Hovering a folded card asks the panel for another frame.
///
/// This is what proves the listener is attached to the card at all: the state it
/// writes lives in the window, and the window notifies the view that owns the
/// element, so a frame the hover causes is a frame the listener caused. It says
/// nothing about the colour — that is [`only_a_folded_card_should_answer_the_pointer`]'s
/// job — and it would still pass if the listener wrote the wrong value.
///
/// The pointer is moved rather than `TestWindowExt::hover`ed for two reasons: that
/// helper is implemented for `Window`, which this harness reaches only inside an
/// `update`, and it draws two frames of its own — it refreshes before and after the
/// move — which would be counted here whatever the listener did. A bare move draws
/// only when something in the frame answered it.
#[gpui_kit::test]
fn hovering_a_folded_card_should_reach_it(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    // Every card is folded, so the pointer has nowhere to land but on a card: an
    // expanded card would put a control under it, and a control answering the
    // pointer would draw a frame that says nothing about the card.
    fold_all_but(window, &panel, &[]);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("MA's card is drawn");
    // The card's padding: inside the card and outside every control it holds.
    let inside = point(card.left() + px(1.5), card.top() + px(1.5));
    let drawn = window.read(|cx| panel.read(cx).rendered_frames());

    window.simulate_mouse_move(inside, None, Modifiers::none());
    window.run_until_parked();

    assert!(
        window.read(|cx| panel.read(cx).rendered_frames()) > drawn,
        "a pointer on a folded card writes its hover state, which the panel draws"
    );
}

/// VOL has no settings rows, so it has no caret and can never be opened: its card
/// is folded for as long as it is drawn, and "every folded card answers the
/// pointer" has to include it.
///
/// This is the case that separates "the card is folded" from "the indicator is
/// marked expanded": VOL starts marked expanded, because the panel opens with every
/// indicator open, and a card that read that flag instead of asking whether it
/// holds anything would draw no hover on the one card that can never hold
/// anything.
#[gpui_kit::test]
fn a_card_with_nothing_to_open_should_still_answer_the_pointer(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    // Every indicator but VOL is folded, so VOL keeps the "expanded" flag the
    // panel opened with and its card is still the only thing there is to it.
    fold_all_but(window, &panel, &[IndicatorId::Vol]);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_VOL")
        .expect("VOL's card is drawn");
    // The pointer settles outside every card first: leaving one is itself a hover
    // change and draws a frame, and a frame counted straight after the move would
    // be that change rather than VOL's answer.
    window.simulate_mouse_move(point(px(0.0), px(0.0)), None, Modifiers::none());
    window.run_until_parked();
    let inside = point(card.left() + px(1.5), card.top() + px(1.5));
    let drawn = window.read(|cx| panel.read(cx).rendered_frames());

    window.simulate_mouse_move(inside, None, Modifiers::none());
    window.run_until_parked();

    assert!(
        window.read(|cx| panel.read(cx).rendered_frames()) > drawn,
        "an indicator with nothing to open is a folded card, so it answers the \
         pointer like the others"
    );
}

/// Hovering an expanded card asks for nothing: it has no listener to hear it.
///
/// The count is taken after the redraw and compared without refreshing again,
/// because a refresh draws everything whether or not anything changed: the
/// question is only whether the hover itself drew a frame. The point is the
/// card's padding, not its centre — the centre of an open card is one of its
/// controls, and a control answering the pointer would draw a frame for a reason
/// that is not the card.
#[gpui_kit::test]
fn an_expanded_card_should_not_be_hoverable(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    // MA is the only expanded card, so the pointer lands on an open one.
    focus_on(window, &panel, IndicatorId::Ma);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("MA's card is drawn");
    let inside = point(card.left() + px(1.5), card.top() + px(1.5));
    let drawn = window.read(|cx| panel.read(cx).rendered_frames());

    window.simulate_mouse_move(inside, None, Modifiers::none());
    window.run_until_parked();

    assert_eq!(
        window.read(|cx| panel.read(cx).rendered_frames()),
        drawn,
        "an expanded card attaches no hover listener, so the pointer over it draws \
         no frame of its own"
    );
}

/// The hover colour takes the 150ms it is declared with to arrive, on both of the
/// card's channels.
///
/// The card's two transitions are what this measures: the colour has to travel,
/// not appear. Nothing about the value or the easing is asked here — that is
/// [`only_a_folded_card_should_answer_the_pointer`]'s job — only how many frames
/// the element keeps asking for after the pointer arrives, and whether what the
/// frames paint is somewhere between the two ends while it does.
///
/// The count is pinned to exactly fifteen, because fifteen is what 150ms is in
/// 10ms steps and the step is deterministic: `advance_clock` moves a simulated
/// clock by a fixed 10ms, not wall-clock time, so the same code asks for the same
/// number of frames on every run. An earlier draft of this test admitted a band of
/// ten to twenty and was wrong to: the band is 100ms to 200ms, so a 120ms
/// transition (twelve frames) and a 200ms one (twenty) both passed it, and neither
/// is the 150ms this panel is written against. Fifteen admits neither, and the
/// neighbouring numbers are far enough away that nothing rounds into it — 100ms is
/// ten frames and 120ms twelve, 200ms twenty and 210ms twenty-one. A zero-duration
/// transition is already at its target in the frame the pointer drew, so it asks for
/// no frame at all and the same count catches it.
///
/// The mid-flight read is what keeps the count honest about *both* channels. The
/// card paints a background and an edge from two separate transitions, and a
/// window asked for a frame by either of them keeps asking: zeroing one of the two
/// would leave the count at fifteen while half the hover snapped into place. Half
/// way through, each painted colour therefore has to be off both of its ends —
/// which is true of a colour that is travelling and false of one that has already
/// arrived, whichever transition it came from.
#[gpui_kit::test]
fn a_card_should_take_the_animation_tick_to_light_up(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    // Every card is folded, so the pointer has nowhere to land but on a card.
    fold_all_but(window, &panel, &[]);
    settle_frames(window);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("MA's card is drawn");
    let (rest_fill, rest_border) = painted_card(window, card);
    let (washed, strong) = window.read(|cx| {
        let roles = &cx.theme().roles;
        (
            roles.surface.sunken.hsla.blend(roles.veil.hover.hsla),
            roles.line.strong.hsla,
        )
    });
    // The card's padding: inside the card and outside every control it holds.
    let inside = point(card.left() + px(1.5), card.top() + px(1.5));
    window.simulate_mouse_move(inside, None, Modifiers::none());
    window.run_until_parked();

    let mut steps = 0;
    let mut midway: Option<(Hsla, Hsla)> = None;
    for step in 0..40 {
        window.executor().advance_clock(Duration::from_millis(10));
        if window.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            break;
        }
        // A frame is drawn between the reads because what a frame asks for is
        // asked for by the frame before it: without the draw, the requests counted
        // are the ones the hover made, over and over.
        window.update(|window, cx| window.render_frame(cx));
        steps = step + 1;
        if steps == 7 {
            midway = Some(painted_card(window, card));
        }
    }

    assert_eq!(
        steps, 15,
        "a 150ms transition advanced in 10ms steps asks for fifteen frames, and \
         asked for {steps}: the clock is simulated, so the count is exact"
    );

    let (fill, border) = midway.expect("the transition was still running half way through");
    assert!(
        !same_color(fill, rest_fill) && !same_color(fill, washed),
        "half way through, the card's fill is between the two ends of its own \
         transition rather than at either: {fill:?} against {rest_fill:?} and {washed:?}"
    );
    assert!(
        !same_color(border, rest_border) && !same_color(border, strong),
        "and its edge is between the two ends of the other one: {border:?} against \
         {rest_border:?} and {strong:?}"
    );
}

/// The colours a card paints are the ones `card_colors` names, in both states the
/// element can be in without a pointer.
///
/// This is the half the pure function cannot state: `card_colors` is what the
/// recipe *is*, and the tests above pin what it returns, but an element that
/// computed it and then painted a literal — or lost the background it was given —
/// would leave those tests green. What is read here is the frame's own quads, so
/// it is the paint the theme's roles reached, not the call that computed them.
///
/// The hovered state is not measured here: reaching it means moving the pointer
/// and waiting out the transition, which [`a_card_should_take_the_animation_tick_to_light_up`]
/// and [`hovering_a_folded_card_should_reach_it`] do between them — but the colour
/// *arrived at* is what [`a_hovered_card_should_paint_its_wash`] reads.
#[gpui_kit::test]
fn a_card_should_paint_the_colours_the_theme_gives_it(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    fold_all_but(window, &panel, &[IndicatorId::Ma]);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("MA's card is drawn");
    let (fill, border) = painted_card(window, card);
    let (surface, divider) = window.read(|cx| {
        (
            cx.theme().roles.surface.sunken.hsla,
            cx.theme().roles.line.divider.hsla,
        )
    });

    assert!(
        same_color(fill, surface),
        "the card's fill is the sunken surface role, not a literal: {fill:?} against \
         {surface:?}"
    );
    assert!(
        same_color(border, divider),
        "and its edge is the divider role: {border:?} against {divider:?}"
    );
}

/// A card under the pointer settles on the wash and the stronger line, painted.
///
/// The hover is the state the call site has to read for the card to answer at all:
/// `card_colors` computed from a hover it ignored would look exactly like this one
/// to every test above, because the resting pair is what it would paint. What is
/// measured is the paint after the transition has run out, so a card that never
/// lifted — or that lifted to a colour of its own — fails here.
#[gpui_kit::test]
fn a_hovered_card_should_paint_its_wash(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    fold_all_but(window, &panel, &[]);
    settle_frames(window);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("MA's card is drawn");
    let inside = point(card.left() + px(1.5), card.top() + px(1.5));
    window.simulate_mouse_move(inside, None, Modifiers::none());
    window.run_until_parked();
    // Past the transition's 150ms, so what is read is the colour it arrived at.
    window.executor().advance_clock(Duration::from_millis(400));
    window.update(|window, cx| window.render_frame(cx));

    let (fill, border) = painted_card(window, card);
    let (washed, strong) = window.read(|cx| {
        let roles = &cx.theme().roles;
        (
            roles.surface.sunken.hsla.blend(roles.veil.hover.hsla),
            roles.line.strong.hsla,
        )
    });

    assert!(
        same_color(fill, washed),
        "a hovered card is painted in its surface with the theme's veil over it: \
         {fill:?} against {washed:?}"
    );
    assert!(
        same_color(border, strong),
        "and its edge is the stronger line, the role the pointer is drawn with \
         everywhere else: {border:?} against {strong:?}"
    );
}

/// The arrow sits halfway between the card's left border and the square the
/// checkbox draws, whichever of those two a reader takes for the box's edge.
///
/// This is the alignment the user asked for after seeing the panel, and it is not
/// where the arrow started: centring it in its own 16px slot put its centre 8px
/// from the slot's left edge, which is 21 in the window and visibly left of the
/// midpoint. What is measured is the glyph, not the slot — a caret inset inside a
/// fixed-size slot moves the glyph and leaves the slot exactly where it was, so the
/// slot's box cannot answer this question.
///
/// The two endpoints are the card's own left border (8) and the checkbox's left
/// edge (33, which is where the square's *box* starts); the midpoint is 20.5. The
/// square inside the checkbox is 4px further in, at 37, and the midpoint of the
/// span the eye reads is 22.5 — the number the arrow is actually placed at. Both
/// are checked, the second one tightly, because a change that moved the arrow back
/// to the slot's centre would fail it by 1.5px.
#[gpui_kit::test]
fn the_caret_should_sit_between_the_cards_edge_and_the_checkbox(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let card = window
        .debug_bounds("TEST_PANEL_CARD_MA")
        .expect("MA's card is drawn");
    let glyph = window
        .debug_bounds("TEST_PANEL_CARET_GLYPH_MA_DOWN")
        .expect("an expanded MA draws the down arrow");
    let toggle = window
        .debug_bounds("TEST_PANEL_NODE_TOGGLE_MA")
        .expect("MA's node row draws the checkbox that enables it");

    let centre = glyph.center().x.as_f32();
    // The checkbox's own left edge is where its *box* starts; the line the eye
    // reads on the arrow's right is the square inside it, one `px()` of checkbox
    // padding further in. That padding is the same `UnifiedSize::Small` the toggle
    // is drawn at, so it is read from the size the control uses rather than written
    // out again here.
    let square_left = toggle.left().as_f32() + UnifiedSize::Small.px().as_f32();
    let between_lines = (card.left().as_f32() + square_left) / 2.0;
    assert!(
        (centre - between_lines).abs() < 0.5,
        "the arrow is centred at {centre}, and halfway between the card's left \
         edge and the square the checkbox draws is {between_lines}"
    );

    // The checkbox draws 4px of its own padding before the square, and the square
    // is the line the eye reads on the right of the arrow.
    let glyph_height = glyph.size.height.as_f32();
    assert!(
        (glyph_height - 12.0).abs() < 0.5,
        "the arrow is the 12px glyph its size_3 asks for, not a larger one: \
         {glyph_height}px"
    );
}

/// Every settings row starts its first control on the checkbox's vertical line.
///
/// The rows are built by three different functions and two of them draw a 1px
/// error frame that the third does not, so "the first control of a row is under the
/// checkbox" is a property of the constants those functions share rather than of
/// any one of them: a row that counted the frame as padding would be 1px left of
/// the others, and a row that did not count it would be 1px right.
///
/// No single indicator has all three kinds of row — MA has a line list, MACD has
/// period and switch rows and no list — so the two are measured one after the
/// other, each against the checkbox in its own node row.
#[gpui_kit::test]
fn every_settings_row_should_start_on_the_checkboxs_line(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);

    // Both indicators are left open in one call: `fold_all_but` toggles every
    // indicator it is not told to keep, so opening them one at a time would fold
    // the first while opening the second.
    fold_all_but(window, &panel, &[IndicatorId::Ma, IndicatorId::Macd]);
    let ma_toggle = window
        .debug_bounds("TEST_PANEL_NODE_TOGGLE_MA")
        .expect("MA's node row draws the checkbox that enables it");
    let line_field = window
        .debug_bounds("TEST_PANEL_PERIOD_FIELD_MA_0")
        .expect("MA's line list draws a field on its first line");
    assert!(
        (line_field.left().as_f32() - ma_toggle.left().as_f32()).abs() <= 1.0,
        "a line row's field starts at {}, and the checkbox that enables the \
         indicator starts at {}: a settings row is a column under the title, not \
         a list indented under it",
        line_field.left().as_f32(),
        ma_toggle.left().as_f32()
    );

    let macd_toggle = window
        .debug_bounds("TEST_PANEL_NODE_TOGGLE_MACD")
        .expect("MACD's node row draws the checkbox that enables it");
    let period_label = window
        .debug_bounds("TEST_PANEL_PERIOD_LABEL_MACD_0")
        .expect("MACD's first period row draws its label");
    // A switch row's first control is the checkbox it draws; the row's own box
    // starts one border further out, so the toggle is what has to land on the line.
    let switch_toggle = window
        .debug_bounds("TEST_PANEL_SWITCH_TOGGLE_MACD_DIF")
        .expect("MACD's DIF switch row draws its checkbox");

    let line = macd_toggle.left().as_f32();
    assert!(
        (period_label.left().as_f32() - line).abs() <= 1.0,
        "a period row's label starts at {}, and the checkbox starts at {line}",
        period_label.left().as_f32()
    );
    assert!(
        (switch_toggle.left().as_f32() - line).abs() <= 1.0,
        "a switch row's checkbox starts at {}, and the node row's checkbox starts \
         at {line}",
        switch_toggle.left().as_f32()
    );
}

/// The arrow is drawn on the row's centre line, not above or below the controls
/// beside it.
///
/// The three things on the row are boxes of three different heights — the arrow's
/// slot, the checkbox's 20px control and the title's line box — so a row that
/// centred some of them and not others would still look almost right. They all
/// have to share one centre, which is what makes the arrow read as part of the row
/// rather than as something floating between two lines.
#[gpui_kit::test]
fn the_caret_should_be_drawn_on_the_node_rows_centre_line(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);
    focus_on(window, &panel, IndicatorId::Ma);

    let glyph = window
        .debug_bounds("TEST_PANEL_CARET_GLYPH_MA_DOWN")
        .expect("an expanded MA draws the down arrow");
    let toggle = window
        .debug_bounds("TEST_PANEL_NODE_TOGGLE_MA")
        .expect("MA's node row draws the checkbox that enables it");
    let title = window
        .debug_bounds("TEST_PANEL_NODE_TITLE_MA")
        .expect("MA's node row draws its title");

    let glyph_centre = glyph.center().y.as_f32();
    let toggle_centre = toggle.center().y.as_f32();
    let title_centre = title.center().y.as_f32();

    assert!(
        (glyph_centre - toggle_centre).abs() <= 0.5,
        "the arrow's centre is {glyph_centre} and the checkbox's is \
         {toggle_centre}: the row centres them on one line"
    );
    assert!(
        (glyph_centre - title_centre).abs() <= 1.0,
        "and the title's centre is {title_centre}, half a pixel off at most: the \
         three boxes are different heights but one centre line"
    );
}

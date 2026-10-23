//! Tests for the indicator settings panel.
//!
//! The panel is an entity of its own, drawn inside a window but outside the
//! framework's pane, so what these tests drive is what a user drives: the
//! elements it draws, the fields it holds, and the configuration it writes back
//! through the widget.

use base_ui::{
    color_picker::{ColorPicker, ColorPickerEvent},
    icon::IconKind,
    input::{InputState, InputStateEvent},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Context, Entity, Hsla, IntoElement, Modifiers, ParentElement, Render, SharedString,
    Styled, TestAppContext, VisualTestContext, Window, div, hsla, point, px, size,
};

use super::{
    colors::{self, ColorKey},
    inputs, rows,
    view::{IndicatorPanel, PANEL_WIDTH},
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

/// Folds every indicator but `only`.
///
/// The panel opens with all of them expanded, and every expanded line list draws
/// the same test selectors; folding the others is what makes one selector name
/// one list. It is also what lets a test say "this is MA's delete button".
fn focus_on(window: &mut VisualTestContext, panel: &Entity<IndicatorPanel>, only: IndicatorId) {
    window.update(|_, cx| {
        for definition in INDICATOR_CATALOG {
            if definition.id == only {
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
        (field.origin.x - (row.origin.x + px(rows::INDENT)))
            .as_f32()
            .abs()
            <= 1.0,
        "the field starts at the row's left edge plus the indent: {field:?} vs {row:?}"
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

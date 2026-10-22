//! Tests for the indicator settings panel.
//!
//! The panel is an entity of its own, drawn inside a window but outside the
//! framework's pane, so what these tests drive is what a user drives: the
//! elements it draws, the fields it holds, and the configuration it writes back
//! through the widget.

use base_ui::{
    input::{InputState, InputStateEvent},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    TestAppContext, VisualTestContext, Window, div,
};

use super::{inputs, rows, view::IndicatorPanel};
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
        div().size_full().child(self.panel.clone())
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
#[gpui_kit::test]
fn the_volume_indicator_should_have_no_caret(cx: &mut TestAppContext) {
    let (_built, panel) = panel_over(cx);
    let window = draw(cx, &panel);

    assert!(
        window.debug_bounds("TEST_PANEL_CARET_MA").is_some(),
        "an indicator with settings rows is drawn with a caret"
    );
    assert!(
        window.debug_bounds("TEST_PANEL_CARET_VOL").is_none(),
        "volume declares no rows, so there is nothing for a caret to open"
    );
}

/// The last line of a list cannot be removed: its delete button is drawn
/// disabled and wired to nothing.
#[gpui_kit::test]
fn the_last_line_of_a_list_should_not_be_removable(cx: &mut TestAppContext) {
    let (built, panel) = panel_over(cx);
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
    assert_eq!(
        ma_periods(window, &built.widget),
        vec![20],
        "two removals leave the last line"
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

    // The button is disabled and carries no handler, but the configuration
    // refuses the removal as well: an action that slipped past the button must
    // not leave a list the settings type says cannot be empty.
    window.update(|_, cx| {
        panel.update(cx, |panel, cx| panel.remove_line(IndicatorId::Ma, 0, cx));
    });
    redraw(window);
    assert_eq!(
        ma_periods(window, &built.widget),
        vec![20],
        "the configuration refuses to empty the list"
    );

    press(window, "TEST_PANEL_DELETE_LINE");
    assert_eq!(
        ma_periods(window, &built.widget),
        vec![20],
        "the last line's button does nothing"
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

//! The rows the panel renders: one per configured indicator and one per parameter.
//!
//! Everything here is a free function over the values a row is drawn from: the
//! panel owns the state and the configuration, and a row only needs to know what
//! it shows and which panel method an interaction calls. Every colour is a
//! `ColorPicker`, never a swatch: a line list draws one per line on that line's
//! row, and a switch row draws its own for the line colour it controls. The
//! dimensions the design fixes live here too, beside the rows that use them.
//!
//! A row is not drawn on the panel: it is drawn inside the card of the indicator
//! it belongs to (see `view::card`). The card's padding is the inset a row starts
//! from — the node row's left edge sits one card border plus that padding inside
//! the card — and a settings row indents its first control by [`SETTING_INSET`] so
//! that it lands on the same vertical line as the checkbox that enables the
//! indicator above it.

use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    input::Input,
    interactive::InteractiveVariantExt,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    toggle::Toggle,
    tooltip::{Tooltip, TooltipExt},
};
use gpui_kit::{
    AnyElement, App, ElementId, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement,
    Role, SharedString, StatefulInteractiveElement, Styled, WeakEntity, base::Placement, div, hsla,
    prelude::FluentBuilder, px,
};
use i18n::t;

use super::{
    colors::{ColorKey, LinePicker},
    inputs::{InputKey, LineInput},
    view::IndicatorPanel,
};
use crate::indicator::{
    IndicatorDefinition, IndicatorId, IndicatorPane, LineListConfig, SettingControl, SettingKey,
    SettingRow,
};

/// The height of an indicator's node row.
const NODE_ROW_HEIGHT: f32 = 28.0;
/// The height of one settings row.
const SETTING_ROW_HEIGHT: f32 = 32.0;
/// The height of the hint under a field that does not hold a period.
const HINT_ROW_HEIGHT: f32 = 18.0;
/// How far the hint is inset past the control it explains.
const HINT_INSET: f32 = 4.0;
/// The height of the row holding the "+" button.
const ADD_ROW_HEIGHT: f32 = 28.0;
/// The gap between the caret and the checkbox that enables an indicator.
const NODE_GAP: f32 = 4.0;
/// The size of the caret that opens and folds an indicator's settings.
const CARET_SIZE: f32 = 16.0;
/// How far the arrow sits from the left edge of the slot that holds it.
///
/// The arrow is centred between the two lines the eye reads around it rather than
/// between the row's own edges: the card's border on its left, and the square
/// inside the checkbox on its right.
///
/// In the panel's coordinates the card's border is at 8 and the row it draws starts
/// at 13, one border and one padding later. The checkbox starts four pixels after
/// the 16px slot — 33 — and draws four pixels of its own padding before its square,
/// so the square's line is at 37. Halfway between 8 and 37 is 22.5, which is where
/// the arrow has to be. The slot spans 13 to 29, so a 12px glyph centred in it would
/// sit at 21: three pixels short. Insetting the slot on its left by three leaves 13
/// pixels for the glyph to centre in, which puts its centre at 13 + 3 + 6.5 = 22.5.
///
/// A padding rather than a width: `.size()` is the outer box, so the slot keeps its
/// 16px and the toggle after it does not move. Aligning the arrow must not shift the
/// checkbox the rest of the row is measured against.
const CARET_INSET: f32 = 3.0;
/// How far a settings row insets its first control.
///
/// The node row puts the checkbox that enables the indicator this far in — a caret
/// slot, then [`NODE_GAP`] — so a settings row whose first control starts at the
/// same x reads as a column under one title rather than as a list indented under
/// it. A text label, an `Input` and a checkbox all line up on this one line.
///
/// `pub(super)` rather than private, which makes it visible to the whole `panel`
/// module: the panel's tests read it as the offset a settings row's first control
/// starts at, and that is what makes "the control is on the left, and under the
/// checkbox" a measurement rather than a description. A literal 20.0 in the test
/// would pin the number instead of the row that places its children by it.
pub(super) const SETTING_INSET: f32 = CARET_SIZE + NODE_GAP;
/// [`SETTING_INSET`] for a row that draws the frame marking a field in error.
///
/// One pixel less, because that frame is part of the box the padding is measured
/// from: a period row and a line row both draw it, and their padding is what is
/// left of the inset once the frame has taken its pixel. Stating both is what keeps
/// the first control of every kind of settings row on one vertical line, whatever
/// each row draws around itself.
pub(super) const FRAMED_INSET: f32 = SETTING_INSET - 1.0;
/// The width of one period field.
///
/// `pub(super)` for the same reason [`SETTING_INSET`] is: a test measures the field
/// against the width the row gives it, so the number is stated once, beside the
/// row that applies it.
pub(super) const PERIOD_FIELD_WIDTH: f32 = 60.0;
/// The width of a row's delete button.
const DELETE_BUTTON_WIDTH: f32 = 24.0;

/// A section heading: "Main chart" or "Sub chart".
///
/// The caption is its own element so that the box the selector names is the box
/// the text is laid out in: the row around it is a fixed height, which would say
/// nothing about the size of the text inside it.
///
/// The selector names the pane the heading introduces, because the panel draws
/// two of them and a test that measured "the section heading" would otherwise
/// answer about whichever one the frame painted last.
pub(super) fn section_title(pane: IndicatorPane, label: SharedString, cx: &App) -> AnyElement {
    div()
        .h(px(NODE_ROW_HEIGHT))
        .flex()
        .items_center()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().roles.fg.muted.hsla)
        .child(
            div()
                .debug_selector(move || section_title_selector(pane))
                .child(label),
        )
        .into_any_element()
}

/// One indicator's node row: the caret, the checkbox that enables it and the
/// indicator's name.
///
/// The row exists for every indicator, and it is always the first child of that
/// indicator's card: an indicator with no settings is a card holding this row and
/// nothing else. The caret exists only for one that has settings to show, and it
/// is not a control of its own: the whole row is what opens and folds them, so it
/// is the row that states whether they are open.
pub(super) fn node_row(
    index: usize,
    definition: &'static IndicatorDefinition,
    enabled: bool,
    expanded: bool,
    panel: &WeakEntity<IndicatorPanel>,
    cx: &App,
) -> AnyElement {
    let caption = t!(cx, definition.name_key);
    let expandable = !definition.rows.is_empty();
    let id = definition.id;
    let row_id = node_id(index);
    let caret = if expandable {
        let icon = caret_icon(expanded);
        div()
            .debug_selector(move || caret_selector(id, icon))
            .flex()
            .items_center()
            .justify_center()
            .size(px(CARET_SIZE))
            // Inside the slot, so the checkbox beside it does not move. The slot
            // keeps its 16px and the glyph centres itself in what is left, which is
            // what puts the arrow where the eye reads the row's centre.
            .pl(px(CARET_INSET))
            .child(
                div()
                    .debug_selector(move || caret_glyph_selector(id, icon))
                    .flex_none()
                    .child(Icon::new(icon).size_3()),
            )
            .into_any_element()
    } else {
        // The title of an indicator without a caret keeps the caret's space, so
        // that every title in the panel starts at the same x.
        div().size(px(CARET_SIZE)).into_any_element()
    };
    let toggle_panel = panel.clone();
    let expand_panel = panel.clone();

    div()
        .id(row_id.clone())
        // The row is an expandable node, and accesskit only reports `aria_*`
        // for an element that declares a role. A tree item is the role that
        // carries both the expanded and the selected state.
        .role(Role::TreeItem)
        .h(px(NODE_ROW_HEIGHT))
        .flex()
        .items_center()
        .gap_1()
        .aria_label(caption.clone())
        // The checkbox's state is the indicator's "selected" state: the node
        // is what a reader lands on, and the toggle inside it is its control.
        .aria_selected(enabled)
        .when(expandable, |row| {
            row.cursor_pointer()
                .aria_expanded(expanded)
                .on_click(move |_, _, cx| {
                    let _ = expand_panel.update(cx, |panel, cx| panel.toggle_expanded(id, cx));
                })
        })
        .child(caret)
        .child(
            Toggle::new((row_id, "enabled"))
                .debug_selector(move || node_toggle_selector(id))
                .checkbox()
                .small()
                .checked(enabled)
                .aria_label(caption.clone())
                .on_change(move |checked, _, cx| {
                    // Clicking the checkbox is not a second way to open the
                    // settings under it.
                    cx.stop_propagation();
                    let _ = toggle_panel.update(cx, |panel, cx| panel.set_enabled(id, checked, cx));
                }),
        )
        .child(
            div()
                .debug_selector(move || node_title_selector(id))
                .text_xs()
                .child(caption),
        )
        .into_any_element()
}

/// One `Period` row: its label on the left, its field on the right, and the hint
/// under it while the field does not hold a period.
pub(super) fn period_row(
    row_index: usize,
    row: &SettingRow,
    input: &LineInput,
    invalid: bool,
    cx: &App,
) -> AnyElement {
    let caption = t!(cx, row.label_key);
    let label_selector = period_label_selector(input.id, row_index);

    div()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(SETTING_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap_2()
                // The frame this row draws is part of the inset, so the padding is
                // what is left of it: the label lands on the checkbox's line either
                // way, whether or not the row is in error.
                .pl(px(FRAMED_INSET))
                .border_1()
                .border_color(border_color(invalid, cx))
                .child(
                    div()
                        .debug_selector(move || label_selector)
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .child(caption.clone()),
                )
                .child(
                    div()
                        .id(ElementId::NamedInteger(
                            SharedString::new_static("CandlestickIndicatorPeriodField"),
                            input.slot,
                        ))
                        .w(px(PERIOD_FIELD_WIDTH))
                        .aria_label(caption)
                        .child(Input::new(&input.entity).small()),
                ),
        )
        .when(invalid, |row| row.child(invalid_hint(cx)))
        .into_any_element()
}

/// One `Switch` row: the checkbox and the name of what it switches on the left,
/// the picker for that line's colour on the right.
///
/// The toggle takes the free width and the picker keeps its own, so the picker
/// sits at the row's right edge rather than beside the label. MACD's histogram is
/// not configurable, which is why only the rows the catalogue gives a colour have
/// a picker.
pub(super) fn switch_row(
    index: usize,
    id: IndicatorId,
    row: &SettingRow,
    on: bool,
    picker: &LinePicker,
    panel: &WeakEntity<IndicatorPanel>,
    cx: &App,
) -> AnyElement {
    let caption = t!(cx, row.label_key);
    let color = t!(cx, "widget.candlestick.indicator.line_color");
    let key = row.key;
    let row_selector = switch_row_selector(picker.key());
    let toggle_selector = switch_toggle_selector(picker.key());
    let panel = panel.clone();

    div()
        .debug_selector(move || row_selector)
        .h(px(SETTING_ROW_HEIGHT))
        .flex()
        .items_center()
        .gap_2()
        // No frame on this row, so the whole inset is padding.
        .pl(px(SETTING_INSET))
        .child(
            div().flex_1().min_w_0().child(
                Toggle::new((node_id(index), row.label_key))
                    .debug_selector(move || toggle_selector)
                    .checkbox()
                    .small()
                    .checked(on)
                    .label(caption.clone())
                    .aria_label(caption)
                    .on_change(move |checked, _, cx| {
                        let _ =
                            panel.update(cx, |panel, cx| panel.set_switch(id, key, checked, cx));
                    }),
            ),
        )
        .child(color_control(picker, color_selector(picker.key()), color))
        .into_any_element()
}

/// One line list: a row per line, and the button that adds another.
///
/// The rows are returned separately so the card can draw them in order after the
/// node row; the "+" button comes last, which is what "at the bottom of the list"
/// means here. Only an expanded indicator draws a line list at all, so the button
/// needs no visibility of its own.
pub(super) fn line_list(
    index: usize,
    id: IndicatorId,
    row: &SettingRow,
    lines: &LineListConfig,
    inputs: &[LineInput],
    pickers: &[LinePicker],
    invalid: Option<InputKey>,
    panel: &WeakEntity<IndicatorPanel>,
    cx: &App,
) -> Vec<AnyElement> {
    let SettingControl::LineList { min_rows } = row.control else {
        return Vec::new();
    };
    let can_remove = can_remove_line(min_rows, lines.rows.len());
    let mut column = div()
        .debug_selector(|| "TEST_PANEL_LINE_LIST".into())
        .id((node_id(index), row.label_key))
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .aria_label(t!(cx, row.label_key));
    for line_index in 0..lines.rows.len() {
        let key = row.key;
        let color_key = ColorKey {
            id,
            row: Some(line_index),
            key,
        };
        let Some(input) = inputs
            .iter()
            .find(|input| input.id == id && input.row == Some(line_index) && input.key == key)
        else {
            continue;
        };
        let Some(picker) = pickers.iter().find(|picker| picker.key() == color_key) else {
            continue;
        };
        let line_invalid = invalid
            == Some(InputKey {
                id,
                row: Some(line_index),
                key,
            });
        column = column.child(line_row(
            line_index,
            id,
            input,
            picker,
            can_remove,
            line_invalid,
            panel,
            cx,
        ));
    }

    vec![
        column.into_any_element(),
        add_line_row(index, id, panel, cx),
    ]
}

/// One line of a line list: its period field, the picker that sets its colour,
/// and the button that deletes it.
///
/// The field sits at the row's left, under the list's indent, and the controls
/// sit at its right: a spacer takes the width between them, so the picker and the
/// delete button stay at the edge whether or not the delete button is drawn. A
/// spacer rather than `justify_between` because the number of controls varies —
/// the last line of a list has no delete button — and `justify_between` would
/// move the picker to the middle of the row exactly when the button is missing.
///
/// The picker is the control, not a swatch beside one. A line list draws exactly
/// one picker per line, on that line's row; MACD's and KDJ's switch rows draw
/// their own pickers for the colours those rows control. The two never share an
/// entity, so no colour has two editors.
fn line_row(
    line_index: usize,
    id: IndicatorId,
    input: &LineInput,
    picker: &LinePicker,
    can_remove: bool,
    invalid: bool,
    panel: &WeakEntity<IndicatorPanel>,
    cx: &App,
) -> AnyElement {
    let period = t!(cx, "widget.candlestick.indicator.period");
    let remove = t!(cx, "widget.candlestick.indicator.remove_line");
    let color = t!(cx, "widget.candlestick.indicator.line_color");
    let delete_panel = panel.clone();
    let row_selector = line_row_selector(id, line_index);
    let field_selector = period_field_selector(id, line_index);
    let delete_selector = delete_button_selector(id, line_index);

    div()
        .flex()
        .flex_col()
        .child(
            div()
                .debug_selector(move || row_selector)
                .h(px(SETTING_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap_2()
                // The same inset a period row uses, and for the same reason: this
                // row draws the frame too, so its padding is what is left of it.
                .pl(px(FRAMED_INSET))
                .border_1()
                .border_color(border_color(invalid, cx))
                .child(
                    div()
                        .debug_selector(move || field_selector)
                        .id(ElementId::NamedInteger(
                            SharedString::new_static("CandlestickIndicatorLineField"),
                            input.slot,
                        ))
                        .flex_none()
                        .w(px(PERIOD_FIELD_WIDTH))
                        .aria_label(period)
                        .child(Input::new(&input.entity).small()),
                )
                .child(div().flex_1())
                .child(color_control(picker, color_selector(picker.key()), color))
                // The last line of a list has no delete button at all: a drawn
                // but disabled one would offer a control that can never act.
                .when(can_remove, |row| {
                    row.child(
                        Button::new(ElementId::NamedInteger(
                            SharedString::new_static("CandlestickIndicatorDeleteLine"),
                            input.slot,
                        ))
                        .debug_selector(move || delete_selector)
                        .ghost()
                        .small()
                        .w(px(DELETE_BUTTON_WIDTH))
                        .aria_label(remove.clone())
                        .with_tooltip(remove)
                        .on_click(move |_, _, cx| {
                            let _ = delete_panel.update(cx, |panel, cx| {
                                panel.remove_line(id, line_index, cx);
                            });
                        })
                        .child(Icon::new(IconKind::Trash).size_3()),
                    )
                }),
        )
        .when(invalid, |row| row.child(invalid_hint(cx)))
        .into_any_element()
}

/// The "+" row: one button across the list's width, with no disabled state to
/// reach — a list has no maximum length.
///
/// The caption is centred rather than left at the button's 4px padding: the
/// button spans the whole row, so a caption at its left edge reads like a field
/// label instead of the action that adds a line.
fn add_line_row(
    index: usize,
    id: IndicatorId,
    panel: &WeakEntity<IndicatorPanel>,
    cx: &App,
) -> AnyElement {
    let label = t!(cx, "widget.candlestick.indicator.add_line");
    let panel = panel.clone();

    div()
        .debug_selector(|| "TEST_PANEL_ADD_LINE".into())
        .w_full()
        .h(px(ADD_ROW_HEIGHT))
        // The button is the row's first control, so it starts on the same line as
        // every other settings row's: the list it grows is read as one column.
        .pl(px(SETTING_INSET))
        .child(
            Button::new(ElementId::NamedInteger(
                SharedString::new_static("CandlestickIndicatorAddLine"),
                index as u64,
            ))
            .debug_selector(|| "TEST_PANEL_ADD_LINE_BUTTON".into())
            .outline()
            .small()
            .w_full()
            .justify_center()
            .aria_label(label.clone())
            .with_tooltip(label.clone())
            .on_click(move |_, _, cx| {
                let _ = panel.update(cx, |panel, cx| panel.add_line(id, cx));
            })
            .child(
                div()
                    .debug_selector(|| "TEST_PANEL_ADD_LINE_LABEL".into())
                    .flex_none()
                    .child(label),
            ),
        )
        .into_any_element()
}

/// Whether a line list with `rows` lines may lose one of them.
///
/// A line list never goes below the minimum its catalogue row declares, and at
/// that minimum the row's delete button is not drawn at all: nothing in the row
/// can be pressed to empty the list, and the configuration refuses the removal
/// as well.
pub(super) fn can_remove_line(min_rows: usize, rows: usize) -> bool {
    rows > min_rows
}

/// The icon that states whether an indicator's settings are open.
///
/// This is the one place the direction is decided: the caret points down while
/// the settings are drawn and right while they are folded, so the same control
/// says both "this is open" and "this opens".
pub(super) fn caret_icon(expanded: bool) -> IconKind {
    if expanded {
        IconKind::CaretDown
    } else {
        IconKind::CaretRight
    }
}

/// The picker of one colour setting, wrapped in its tooltip and its test selector.
///
/// The tooltip is placed above its trigger because the picker opens its panel
/// below it: a tooltip below would be covered by the very panel it explains the
/// moment the user opens the picker.
fn color_control(picker: &LinePicker, selector: String, label: SharedString) -> AnyElement {
    div()
        .debug_selector(move || selector)
        .flex_none()
        .child(
            Tooltip::new(
                ElementId::NamedInteger(
                    SharedString::new_static("CandlestickIndicatorColorTooltip"),
                    picker.slot(),
                ),
                picker.entity().clone(),
            )
            .placement(Placement::Top)
            .content(label),
        )
        .into_any_element()
}

/// The hint under a field whose text is not a period it may take.
///
/// It starts [`HINT_INSET`] past the label it explains, which is the row's first
/// control and therefore begins at [`SETTING_INSET`] from the row's edge — the
/// frame the row may also draw is already accounted for in that line.
fn invalid_hint(cx: &App) -> AnyElement {
    div()
        .debug_selector(|| "TEST_PANEL_INVALID_PERIOD".into())
        .h(px(HINT_ROW_HEIGHT))
        .pl(px(SETTING_INSET + HINT_INSET))
        .text_xs()
        .text_color(cx.theme().roles.status.danger.base.hsla)
        .child(t!(cx, "widget.candlestick.indicator.period_invalid"))
        .into_any_element()
}

/// The border of a row that holds a period: the error role while the text is
/// not a period, and a fully transparent line otherwise so that the row does not
/// move when the frame appears.
fn border_color(invalid: bool, cx: &App) -> Hsla {
    if invalid {
        cx.theme().roles.status.danger.base.hsla
    } else {
        hsla(0.0, 0.0, 0.0, 0.0)
    }
}

/// The element id of one indicator's card.
///
/// A card needs an id of its own because it carries state — the pointer's hover,
/// and the two transitions that follow it — and because that state must survive the
/// panel being redrawn: an id that moved with the frame would reset the transition
/// on every keystroke in a field under it.
pub(super) fn card_id(index: usize) -> ElementId {
    ElementId::NamedInteger(
        SharedString::new_static("CandlestickIndicatorCard"),
        index as u64,
    )
}

/// The element id of one indicator's node row, and of everything named after it.
fn node_id(index: usize) -> ElementId {
    ElementId::NamedInteger(
        SharedString::new_static("CandlestickIndicator"),
        index as u64,
    )
}

/// The debug selector of one indicator's caret.
///
/// An indicator with no settings rows is drawn without a caret, which is what a
/// test asks this selector for. The direction is part of the name, and it is read
/// from the icon the row is about to draw rather than from the flag it was given:
/// a selector that said "right" because the row was folded would still say it
/// after the row started drawing a left caret, which is the bug the selector is
/// there to catch.
///
/// The names are written out rather than derived from the id's `Debug` spelling:
/// a selector a test reads should not depend on how a derive happens to print.
fn caret_selector(id: IndicatorId, icon: IconKind) -> String {
    let direction = match icon {
        IconKind::CaretDown => "DOWN",
        IconKind::CaretRight => "RIGHT",
        IconKind::CaretLeft => "LEFT",
        _ => panic!("only a caret names a caret selector"),
    };
    format!("TEST_PANEL_CARET_{}_{direction}", indicator_name(id))
}

/// The debug selector of one indicator's card.
///
/// The box a test measures the node row and the settings against: everything the
/// indicator draws is inside this card, which is what lets a test say "these rows
/// belong to this indicator" rather than "these rows are somewhere in the panel".
pub(super) fn card_selector(id: IndicatorId) -> String {
    format!("TEST_PANEL_CARD_{}", indicator_name(id))
}

/// The debug selector of the arrow glyph itself, inside the slot that holds it.
///
/// The slot is a fixed 16px and the arrow inside it is 12, so the slot's own box
/// says nothing about where the arrow is: a caret inset within its slot moves the
/// glyph and leaves the slot exactly where it was. What the alignment is about is
/// the glyph, so the glyph is what gets the selector.
fn caret_glyph_selector(id: IndicatorId, icon: IconKind) -> String {
    let direction = match icon {
        IconKind::CaretDown => "DOWN",
        IconKind::CaretRight => "RIGHT",
        IconKind::CaretLeft => "LEFT",
        _ => panic!("only a caret names a caret glyph selector"),
    };
    format!("TEST_PANEL_CARET_GLYPH_{}_{direction}", indicator_name(id))
}

/// The debug selector of the checkbox that enables one indicator.
///
/// A settings row aligns its first control with this box's left edge, so a test
/// that says "the parameter starts under the checkbox" has to be able to ask where
/// the checkbox is.
fn node_toggle_selector(id: IndicatorId) -> String {
    format!("TEST_PANEL_NODE_TOGGLE_{}", indicator_name(id))
}

/// The debug selector of one indicator's title in its node row.
///
/// The title is measured against a section heading: the two captions are the
/// same kind of text at two sizes, so their boxes are what states the sizes.
fn node_title_selector(id: IndicatorId) -> String {
    format!("TEST_PANEL_NODE_TITLE_{}", indicator_name(id))
}

/// The debug selector of one `Period` row's label.
///
/// Named after the indicator and the row's place in its catalogue entry: a
/// period row has no line index, and two of an indicator's rows can both be
/// periods (MACD has three), so the position is what tells them apart.
fn period_label_selector(id: IndicatorId, row_index: usize) -> String {
    let name = indicator_name(id);
    format!("TEST_PANEL_PERIOD_LABEL_{name}_{row_index}")
}

/// The debug selector of the heading that introduces one region of the chart.
fn section_title_selector(pane: IndicatorPane) -> String {
    let pane = match pane {
        IndicatorPane::Main => "MAIN",
        IndicatorPane::Sub => "SUB",
    };
    format!("TEST_PANEL_SECTION_TITLE_{pane}")
}

/// The debug selector of one colour picker.
///
/// A line's picker is named after its indicator and its line, and a switch row's
/// after its indicator and the line key it controls. The indicator name keeps the
/// selectors apart even though two indicators' line lists both start at row 0.
fn color_selector(key: ColorKey) -> String {
    let name = indicator_name(key.id);
    match key.row {
        Some(row) => format!("TEST_PANEL_COLOR_{name}_{row}"),
        None => format!("TEST_PANEL_COLOR_{name}_{}", color_key_name(key.key)),
    }
}

/// The debug selector of one `Switch` row.
///
/// The row is named after the same key its picker is, so a test can ask whether
/// the picker is drawn inside the row and where it sits on it.
fn switch_row_selector(key: ColorKey) -> String {
    let name = indicator_name(key.id);
    format!("TEST_PANEL_SWITCH_ROW_{name}_{}", color_key_name(key.key))
}

/// The debug selector of the checkbox inside one `Switch` row.
///
/// The row and its checkbox are two boxes: the row is what the inset places and the
/// toggle is what the first-control alignment is about, so a test that asked only
/// for the row would answer about the wrong box.
fn switch_toggle_selector(key: ColorKey) -> String {
    let name = indicator_name(key.id);
    format!(
        "TEST_PANEL_SWITCH_TOGGLE_{name}_{}",
        color_key_name(key.key)
    )
}

/// The debug selector of one line of a line list.
///
/// The row is what a test measures the controls inside it against: the field's
/// left edge and the controls' right edge are stated relative to it. The line
/// index is part of the name because a list draws as many rows as it has lines.
fn line_row_selector(id: IndicatorId, line_index: usize) -> String {
    let name = indicator_name(id);
    format!("TEST_PANEL_LINE_ROW_{name}_{line_index}")
}

/// The debug selector of the wrapper around one line's period field.
///
/// The wrapper, not the field itself: the field is a `base_ui` input whose box a
/// test already finds under its own selector, and what this row decides is where
/// the wrapper it was given is placed.
fn period_field_selector(id: IndicatorId, line_index: usize) -> String {
    let name = indicator_name(id);
    format!("TEST_PANEL_PERIOD_FIELD_{name}_{line_index}")
}

/// The debug selector of one line's delete button.
///
/// Named after its line like the row it sits on: a list draws one delete button
/// per line, so a selector every one of them shares would answer about whichever
/// of them the frame happened to paint last.
fn delete_button_selector(id: IndicatorId, line_index: usize) -> String {
    let name = indicator_name(id);
    format!("TEST_PANEL_DELETE_LINE_{name}_{line_index}")
}

/// The name a colour key is written under in a test selector.
fn color_key_name(key: SettingKey) -> &'static str {
    match key {
        SettingKey::DifColor => "DIF",
        SettingKey::DeaColor => "DEA",
        SettingKey::KColor => "K",
        SettingKey::DColor => "D",
        SettingKey::JColor => "J",
        _ => panic!("only a colour setting names a colour selector"),
    }
}

/// The short name an indicator is written under in a test selector.
fn indicator_name(id: IndicatorId) -> &'static str {
    match id {
        IndicatorId::Ma => "MA",
        IndicatorId::Ema => "EMA",
        IndicatorId::Vol => "VOL",
        IndicatorId::Macd => "MACD",
        IndicatorId::Rsi => "RSI",
        IndicatorId::Kdj => "KDJ",
    }
}

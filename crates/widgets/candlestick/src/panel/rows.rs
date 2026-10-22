//! The rows the panel renders: one per configured indicator and one per parameter.
//!
//! Everything here is a free function over the values a row is drawn from: the
//! panel owns the state and the configuration, and a row only needs to know what
//! it shows and which panel method an interaction calls. Every colour is a
//! `ColorPicker`, never a swatch: a line list draws one per line on that line's
//! row, and a switch row draws its own for the line colour it controls. The
//! dimensions the design fixes live here too, beside the rows that use them.

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
    Role, SharedString, StatefulInteractiveElement, Styled, WeakEntity,
    base::{Disableable, Placement},
    div, hsla,
    prelude::FluentBuilder,
    px,
};
use i18n::t;

use super::{
    colors::{ColorKey, LinePicker},
    inputs::{InputKey, LineInput},
    view::IndicatorPanel,
};
use crate::indicator::{
    IndicatorDefinition, IndicatorId, LineListConfig, SettingControl, SettingKey, SettingRow,
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
/// One level of indent: a settings row sits one level under its indicator.
const INDENT: f32 = 16.0;
/// The size of the caret that opens and folds an indicator's settings.
const CARET_SIZE: f32 = 16.0;
/// The width of one period field.
const PERIOD_FIELD_WIDTH: f32 = 60.0;
/// The width of a row's delete button.
const DELETE_BUTTON_WIDTH: f32 = 24.0;

/// A section heading: "Main chart" or "Sub chart".
pub(super) fn section_title(label: SharedString, cx: &App) -> AnyElement {
    div()
        .h(px(NODE_ROW_HEIGHT))
        .flex()
        .items_center()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().roles.fg.muted.hsla)
        .child(label)
        .into_any_element()
}

/// One indicator's node row: the caret, the checkbox that enables it and the
/// indicator's name.
///
/// The row exists for every indicator. The caret exists only for one that has
/// settings to show, and it is not a control of its own: the whole row is what
/// opens and folds them, so it is the row that states whether they are open.
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
        div()
            .debug_selector(move || caret_selector(id))
            .flex()
            .items_center()
            .justify_center()
            .size(px(CARET_SIZE))
            .child(
                Icon::new(if expanded {
                    IconKind::CaretDown
                } else {
                    IconKind::CaretLeft
                })
                .size_3(),
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
        .child(div().text_sm().child(caption))
        .into_any_element()
}

/// One `Period` row: its label on the left, its field on the right, and the hint
/// under it while the field does not hold a period.
pub(super) fn period_row(
    row: &SettingRow,
    input: &LineInput,
    invalid: bool,
    cx: &App,
) -> AnyElement {
    let caption = t!(cx, row.label_key);

    div()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(SETTING_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap_2()
                .pl(px(INDENT))
                .border_1()
                .border_color(border_color(invalid, cx))
                .child(div().flex_1().min_w_0().text_sm().child(caption.clone()))
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
    let panel = panel.clone();

    div()
        .debug_selector(move || row_selector)
        .h(px(SETTING_ROW_HEIGHT))
        .flex()
        .items_center()
        .gap_2()
        .pl(px(INDENT))
        .child(
            div().flex_1().min_w_0().child(
                Toggle::new((node_id(index), row.label_key))
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
/// The rows are returned separately so the panel's column can draw them in
/// order; the "+" button comes last, which is what "at the bottom of the list"
/// means here. Only an expanded indicator is drawn at all, so the button needs
/// no visibility of its own.
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

    div()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(SETTING_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap_2()
                .pl(px(INDENT))
                .border_1()
                .border_color(border_color(invalid, cx))
                .child(
                    div()
                        .id(ElementId::NamedInteger(
                            SharedString::new_static("CandlestickIndicatorLineField"),
                            input.slot,
                        ))
                        .w(px(PERIOD_FIELD_WIDTH))
                        .aria_label(period)
                        .child(Input::new(&input.entity).small()),
                )
                .child(color_control(picker, color_selector(picker.key()), color))
                .child(
                    Button::new(ElementId::NamedInteger(
                        SharedString::new_static("CandlestickIndicatorDeleteLine"),
                        input.slot,
                    ))
                    .debug_selector(|| "TEST_PANEL_DELETE_LINE".into())
                    .ghost()
                    .small()
                    .w(px(DELETE_BUTTON_WIDTH))
                    .aria_label(remove.clone())
                    .with_tooltip(remove)
                    .disabled(!can_remove)
                    // A disabled `Button` only looks disabled: the handler it was
                    // given still runs, so the last line's button has none.
                    .when(can_remove, |button| {
                        button.on_click(move |_, _, cx| {
                            let _ = delete_panel.update(cx, |panel, cx| {
                                panel.remove_line(id, line_index, cx);
                            });
                        })
                    })
                    .child(Icon::new(IconKind::Trash).size_3()),
                ),
        )
        .when(invalid, |row| row.child(invalid_hint(cx)))
        .into_any_element()
}

/// The "+" row: one button across the list's width, with no disabled state to
/// reach — a list has no maximum length.
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
        .pl(px(INDENT))
        .child(
            Button::new(ElementId::NamedInteger(
                SharedString::new_static("CandlestickIndicatorAddLine"),
                index as u64,
            ))
            .outline()
            .small()
            .w_full()
            .aria_label(label.clone())
            .with_tooltip(label.clone())
            .on_click(move |_, _, cx| {
                let _ = panel.update(cx, |panel, cx| panel.add_line(id, cx));
            })
            .child(label),
        )
        .into_any_element()
}

/// Whether a line list with `rows` lines may lose one of them.
///
/// A line list never goes below the minimum its catalogue row declares, which is
/// what makes the last line's delete button a disabled button rather than a
/// hidden one: hiding it would move the controls of the row as the list shrinks.
pub(super) fn can_remove_line(min_rows: usize, rows: usize) -> bool {
    rows > min_rows
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
fn invalid_hint(cx: &App) -> AnyElement {
    div()
        .debug_selector(|| "TEST_PANEL_INVALID_PERIOD".into())
        .h(px(HINT_ROW_HEIGHT))
        .pl(px(INDENT + HINT_INSET))
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
/// test asks this selector for. The names are written out rather than derived
/// from the id's `Debug` spelling: a selector a test reads should not depend on
/// how a derive happens to print.
fn caret_selector(id: IndicatorId) -> String {
    format!("TEST_PANEL_CARET_{}", indicator_name(id))
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

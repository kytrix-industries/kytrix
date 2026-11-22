//! The rows the panel shows: one card per configured indicator and one row per parameter.
//!
//! Everything here is a builder over the values a row is drawn from: the panel owns the state and
//! the configuration, and a row only needs to know what it shows and which [`PanelEdit`] an
//! interaction writes. The controls are
//! [`kit`](base_ui::kit) components — a [`Button`] for the caret, the add-line row and the delete
//! buttons, a [`Toggle`] for the checkbox that enables an indicator and for its switches, a
//! [`FieldInput`] for every period field and a [`FieldPicker`] for every configurable colour — so
//! their keys, action handlers and signal scopes are the components' own and the panel only keeps
//! the handles to release.
//!
//! The built rows are **rebuilt as a block**, never patched in place: the tree of cards is a
//! function of `(configuration, language)` — which indicators exist, how many lines a list has,
//! which labels a control carries — and comparing those two values is cheaper and harder to get
//! wrong than keeping every control's copy and count in step. A field that is being typed into is
//! not rebuilt: typing does not change the configuration, and the rebuild only runs when the
//! widget's configuration or the language actually moved.
//!
//! The dimensions the panel fixes live here too, beside the rows that use them: the node row's
//! height, a settings row's height, the width of a period field and the insets the rows line their
//! controls up by. The insets are computed from the kit size steps rather than stated twice: the
//! caret is a small button whose width is its icon plus its own paddings and border, and a settings
//! row insets its first control so that what is *drawn* lands on the vertical line the checkbox's
//! square is on.

use base_ui::{
    kit::{
        button::Button,
        icon::IconKind,
        size::{UnifiedSizable, UnifiedSize},
        toggle::Toggle,
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, resolved::Theme, role::Role},
};
use i18n::t_smol;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    event::Action,
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, signal::Signal},
    paint::{color::Color, primitive::Border, style::StylePatch},
    text::TextStyle,
};
use widget::cx::WidgetCx;

use super::{
    colors::{self, ColorField, ColorKey, FieldPicker, color_labels},
    inputs::{self, FieldInput, InputField, InputKey},
    view::{Control, PanelEdit},
};
use crate::indicator::{
    CandlestickIndicatorConfig, INDICATOR_CATALOG, IndicatorDefinition, IndicatorEntry,
    IndicatorId, IndicatorPane, LineListConfig, SettingControl, SettingKey, SettingRow,
    definition_of,
};

/// The height of an indicator's node row.
const NODE_ROW_HEIGHT: f32 = 28.0;
/// The height of one settings row.
const SETTING_ROW_HEIGHT: f32 = 32.0;
/// The gap between the caret and the checkbox that enables an indicator.
const NODE_GAP: f32 = 4.0;
/// The gap between two controls of a settings row.
const ROW_GAP_X: f32 = 8.0;
/// The gap between two lines of a line list.
const LINE_GAP: f32 = 4.0;
/// The width of one period field.
///
/// `pub(super)` so the panel's tests measure the field against the width the row gives it rather
/// than pinning a second copy of the number.
pub(super) const PERIOD_FIELD_WIDTH: f32 = 60.0;
/// The edge of the caret glyph and of a delete button's glyph.
const ROW_ICON_SIZE: f32 = 12.0;
/// The padding inside one indicator's card, on all four sides.
const CARD_PADDING: f32 = 8.0;
/// The corner radius of one indicator's card.
const CARD_RADIUS: f32 = 4.0;
/// How far the invalid-period hint is inset past the control it explains.
const HINT_INSET: f32 = 4.0;

/// The panel signals the rows' handlers write into.
///
/// A control's callback runs with an application context and no panel, so it cannot change the
/// configuration: it writes what the user did, and the panel's `sync` applies it to the
/// configuration the widget published.
#[derive(Clone, Copy)]
pub(super) struct Wiring {
    /// Committed edits and panel-local actions, drained by the panel's sync.
    pub edits: Signal<Option<PanelEdit>>,
    /// The field currently in error, read by every row's hint and error frame.
    pub invalid: Signal<Option<InputKey>>,
}

/// The controls, action handlers and card nodes one rebuild produced.
///
/// The panel keeps them for as long as the rows are built: the controls have to be synced and
/// eventually released, the handlers unregistered, and the cards are what a pointer is mirrored
/// against.
pub(super) struct BuiltRows {
    pub controls: Vec<Control>,
    pub actions: Vec<Action>,
    pub cards: Vec<(IndicatorId, NodeId)>,
}

/// Builds the whole row tree from `config` and the current language.
///
/// `content` is the panel's column; every card and section heading is built under it. `hover` is
/// one mirror per catalogue indicator, and `expanded` the panel's own open state — both are the
/// panel's fields, handed over so a card's style closure and its settings know what they draw.
pub(super) fn build_rows(
    cx: &mut WidgetCx<'_>,
    config: &CandlestickIndicatorConfig,
    theme: &ThemeHandle,
    wiring: Wiring,
    hover: Vec<(IndicatorId, Signal<bool>)>,
    expanded: Vec<(IndicatorId, bool)>,
    content: NodeId,
) -> Result<BuiltRows, EngineError> {
    let mut rows = Rows {
        cx,
        theme: theme.clone(),
        wiring,
        hover,
        expanded,
        controls: Vec::with_capacity(32),
        actions: Vec::with_capacity(INDICATOR_CATALOG.len() * 2),
        cards: Vec::with_capacity(INDICATOR_CATALOG.len()),
    };
    rows.build_all(config, content)?;
    Ok(BuiltRows {
        controls: rows.controls,
        actions: rows.actions,
        cards: rows.cards,
    })
}

/// The builder one rebuild runs through.
struct Rows<'a, 'b> {
    cx: &'a mut WidgetCx<'b>,
    theme: ThemeHandle,
    wiring: Wiring,
    hover: Vec<(IndicatorId, Signal<bool>)>,
    expanded: Vec<(IndicatorId, bool)>,
    controls: Vec<Control>,
    actions: Vec<Action>,
    cards: Vec<(IndicatorId, NodeId)>,
}

impl Rows<'_, '_> {
    /// Builds the two sections and every card they hold, in catalogue order.
    fn build_all(
        &mut self,
        config: &CandlestickIndicatorConfig,
        content: NodeId,
    ) -> Result<(), EngineError> {
        for pane in [IndicatorPane::Main, IndicatorPane::Sub] {
            self.section_title(pane, content)?;
            for entry in &config.entries {
                let definition = definition_of(entry.id);
                if definition.pane != pane {
                    continue;
                }
                self.card(config, entry, definition, content)?;
            }
        }
        Ok(())
    }

    /// A section heading: "Main chart" or "Sub chart".
    fn section_title(&mut self, pane: IndicatorPane, parent: NodeId) -> Result<(), EngineError> {
        let translate = self.cx.translate().clone();
        let language = self.cx.language();
        let theme = self.theme.clone();
        let holder = NodeBuilder::boxed()
            .id(format_smolstr!(
                "TEST_PANEL_SECTION_TITLE_{}",
                pane_name(pane)
            ))
            .style(
                StyleBuilder::new()
                    .height(NODE_ROW_HEIGHT)
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(self.cx.tree_mut(), parent)?;
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Medium.text_size()).with_weight(500),
        )
        .bind_text(move |read| {
            let _language = *read.read(&language);
            t_smol!(translate, section_key(pane))
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(self.cx.tree_mut(), holder)?;
        Ok(())
    }

    /// One indicator's card: its node row, and its settings while it is open.
    ///
    /// The card is the box that says "these rows belong to this indicator"; a card that is folded
    /// (or an indicator with nothing to configure) answers the pointer with a lighter surface,
    /// while an open card draws no hover at all — its children are controls of their own.
    fn card(
        &mut self,
        config: &CandlestickIndicatorConfig,
        entry: &IndicatorEntry,
        definition: &'static IndicatorDefinition,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let id = definition.id;
        let name = indicator_name(id);
        let border = self.theme.tokens().border_width;
        let open = self.is_expanded(id) && !definition.rows.is_empty();
        let folded = !open;
        let hover = self.hover_signal(id);
        let theme = self.theme.clone();
        let card = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_CARD_{name}"))
            .style(
                StyleBuilder::new()
                    .width_full()
                    .flex_col()
                    .padding(CARD_PADDING)
                    .border(border)
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .rounded(CARD_RADIUS)
                    .border(border, Color::TRANSPARENT)
                    .build(),
            )
            .bind_style(move |read| {
                let (background, border_color) =
                    card_colors(theme.theme(read), folded, folded && *read.read(&hover));
                StylePatch {
                    background: Some(background),
                    border: Some(Border {
                        width: border,
                        color: border_color,
                    }),
                    ..StylePatch::default()
                }
            })
            .build(self.cx.tree_mut(), parent)?;
        self.node_row(definition, entry.enabled, open, card)?;
        if open {
            for (row_index, row) in definition.rows.iter().enumerate() {
                match row.control {
                    SettingControl::LineList { .. } => {
                        let Some(lines) = entry.settings.lines() else {
                            continue;
                        };
                        self.line_list(config, id, row, lines, card)?;
                    }
                    SettingControl::Period { .. } => {
                        let key = InputKey {
                            id,
                            row: None,
                            key: row.key,
                        };
                        let Some(field) = inputs::field_of(config, key) else {
                            continue;
                        };
                        self.period_row(id, row, row_index, field, card)?;
                    }
                    SettingControl::Switch => {
                        self.switch_row(config, id, row, entry, card)?;
                    }
                }
            }
        }
        self.cards.push((id, card));
        Ok(())
    }

    /// One indicator's node row: the caret that opens its settings, the checkbox that enables it
    /// and the indicator's name.
    ///
    /// The row itself is what opens and folds the settings, as it was in the old panel: the caret
    /// is a copy of the same action for a reader who looks for a control, and a click anywhere on
    /// the row that the checkbox did not take toggles the card. An indicator with no settings gets
    /// no caret, so its card is a title that still answers the pointer.
    fn node_row(
        &mut self,
        definition: &'static IndicatorDefinition,
        enabled: bool,
        expanded: bool,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let id = definition.id;
        let name = indicator_name(id);
        let caption = t_smol!(self.cx.translate(), definition.name_key);
        let expandable = !definition.rows.is_empty();
        let mut row = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_NODE_{name}"))
            .style(
                StyleBuilder::new()
                    .height(NODE_ROW_HEIGHT)
                    .flex_row()
                    .items_center()
                    .gap(NODE_GAP)
                    .build(),
            )
            .a11y(A11yOverride {
                role: Some(A11yRole::TreeItem),
                label: Some(caption.clone()),
                ..A11yOverride::default()
            })
            .bind_a11y(move |_read| A11yPatch {
                expanded: expandable.then_some(expanded),
                selected: Some(enabled),
                ..A11yPatch::default()
            });
        if expandable {
            let action = self.register_expand(id, &format_smolstr!("panel.node.{name}.row"));
            row = row.on_click(action);
        }
        let row = row.build(self.cx.tree_mut(), parent)?;
        if expandable {
            let key = self
                .cx
                .unique_key(&format_smolstr!("panel.node.{name}.caret"));
            let edits = self.wiring.edits;
            let window = self.cx.window();
            let caret = Button::new(self.cx.app_mut(), self.theme.clone(), key)
                .ghost()
                .small()
                .icon(caret_icon(expanded))
                .icon_size(ROW_ICON_SIZE)
                .a11y_label(caption.clone())
                .on_click(move |ctx| {
                    ctx.write(&edits, Some(PanelEdit::ToggleExpanded(id)));
                    let _ = ctx.app_mut().request_repaint(window);
                });
            caret.build(self.cx.app_mut(), window, row)?;
            self.controls.push(Control::Button(caret));
        } else {
            // The title of an indicator without a caret keeps the caret's space, so that every
            // title in the panel starts at the same x.
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(caret_width(&self.theme))
                        .height(NODE_ROW_HEIGHT)
                        .flex_none()
                        .build(),
                )
                .build(self.cx.tree_mut(), row)?;
        }
        let key = self
            .cx
            .unique_key(&format_smolstr!("panel.node.{name}.enabled"));
        let edits = self.wiring.edits;
        let window = self.cx.window();
        let toggle = Toggle::new(self.cx.app_mut(), self.theme.clone(), key)
            .checkbox()
            .small()
            .checked(enabled)
            .label(caption)
            .on_change(move |checked, app| {
                app.write(
                    &edits,
                    Some(PanelEdit::SetEnabled {
                        id,
                        enabled: checked,
                    }),
                );
                let _ = app.request_repaint(window);
            });
        toggle.build(self.cx.app_mut(), window, row)?;
        self.controls.push(Control::Toggle(toggle));
        Ok(())
    }

    /// One named period row: its label on the left, its field on the right, and the hint under it
    /// while the field does not hold a period it may take.
    fn period_row(
        &mut self,
        id: IndicatorId,
        row: &SettingRow,
        row_index: usize,
        field: InputField,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let name = indicator_name(id);
        let key = InputKey {
            id,
            row: None,
            key: row.key,
        };
        let caption = t_smol!(self.cx.translate(), row.label_key);
        let border = self.theme.tokens().border_width;
        let holder = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_PERIOD_ROW_{name}_{row_index}"))
            .style(StyleBuilder::new().width_full().flex_col().build())
            .build(self.cx.tree_mut(), parent)?;
        let line = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .height(SETTING_ROW_HEIGHT)
                    .flex_row()
                    .items_center()
                    .gap(ROW_GAP_X)
                    .padding_sides(0.0, 0.0, 0.0, setting_inset(&self.theme))
                    .relative()
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .border(border, Color::TRANSPARENT)
                    .build(),
            )
            .bind_style(invalid_border(&self.theme, self.wiring.invalid, key))
            .build(self.cx.tree_mut(), holder)?;
        self.label_text(
            row.label_key,
            format_smolstr!("TEST_PANEL_PERIOD_LABEL_{name}_{row_index}"),
            line,
        )?;
        let field_node = NodeBuilder::boxed()
            .id(format_smolstr!(
                "TEST_PANEL_PERIOD_FIELD_{name}_{row_index}"
            ))
            .style(
                StyleBuilder::new()
                    .width(PERIOD_FIELD_WIDTH)
                    .flex_none()
                    .build(),
            )
            // The kit input's own accessible name is its action key; the labelled group around
            // it is what tells a reader which setting the field edits.
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                label: Some(caption),
                ..A11yOverride::default()
            })
            .build(self.cx.tree_mut(), line)?;
        let suffix = format_smolstr!("panel.period.{name}.{}", setting_name(row.key));
        let input = FieldInput::build(&mut *self.cx, field_node, &field, suffix.as_str());
        self.controls.push(Control::Input(input));
        self.invalid_hint(
            key,
            format_smolstr!("TEST_PANEL_INVALID_{name}_{row_index}"),
            holder,
        )?;
        Ok(())
    }

    /// One switch row: the checkbox and the name of what it switches on the left, the picker for
    /// that line's colour on the right.
    fn switch_row(
        &mut self,
        config: &CandlestickIndicatorConfig,
        id: IndicatorId,
        row: &SettingRow,
        entry: &IndicatorEntry,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let Some(color_key) = row.color_key else {
            return Ok(());
        };
        let color_key = ColorKey {
            id,
            row: None,
            key: color_key,
        };
        let Some(field) = colors::field_of(config, color_key) else {
            return Ok(());
        };
        let caption = t_smol!(self.cx.translate(), row.label_key);
        let on = entry.settings.switch(row.key).unwrap_or(false);
        let name = indicator_name(id);
        let setting = row.key;
        let holder = NodeBuilder::boxed()
            .id(format_smolstr!(
                "TEST_PANEL_SWITCH_ROW_{name}_{}",
                setting_name(setting)
            ))
            .style(
                StyleBuilder::new()
                    .height(SETTING_ROW_HEIGHT)
                    .flex_row()
                    .items_center()
                    .gap(ROW_GAP_X)
                    .padding_sides(0.0, 0.0, 0.0, checkbox_inset(&self.theme))
                    .build(),
            )
            .build(self.cx.tree_mut(), parent)?;
        self.toggle_row(
            id,
            setting,
            caption,
            on,
            &format_smolstr!("panel.switch.{name}.{}", setting_name(setting)),
            holder,
        )?;
        let labels = color_labels(self.cx.translate());
        let color_holder = NodeBuilder::boxed()
            .id(format_smolstr!(
                "TEST_PANEL_COLOR_{name}_{}",
                setting_name(color_key.key)
            ))
            .style(StyleBuilder::new().flex_none().build())
            .build(self.cx.tree_mut(), holder)?;
        let suffix = format_smolstr!("panel.color.{name}.{}", setting_name(color_key.key));
        let picker = FieldPicker::build(
            &mut *self.cx,
            color_holder,
            &field,
            &labels,
            self.wiring.edits,
            suffix.as_str(),
        )?;
        self.controls.push(Control::Picker(picker));
        Ok(())
    }

    /// Builds a checkbox toggle into `parent` and connects it to one named switch.
    fn toggle_row(
        &mut self,
        id: IndicatorId,
        setting: SettingKey,
        caption: SmolStr,
        on: bool,
        suffix: &str,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let holder = NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .build(self.cx.tree_mut(), parent)?;
        let key = self.cx.unique_key(suffix);
        let edits = self.wiring.edits;
        let window = self.cx.window();
        let toggle = Toggle::new(self.cx.app_mut(), self.theme.clone(), key)
            .checkbox()
            .small()
            .checked(on)
            .label(caption)
            .on_change(move |checked, app| {
                app.write(
                    &edits,
                    Some(PanelEdit::SetSwitch {
                        id,
                        key: setting,
                        on: checked,
                    }),
                );
                let _ = app.request_repaint(window);
            });
        toggle.build(self.cx.app_mut(), window, holder)?;
        self.controls.push(Control::Toggle(toggle));
        Ok(())
    }

    /// One line list: a row per line, and the button that adds another.
    fn line_list(
        &mut self,
        config: &CandlestickIndicatorConfig,
        id: IndicatorId,
        row: &SettingRow,
        lines: &LineListConfig,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let SettingControl::LineList { min_rows } = row.control else {
            return Ok(());
        };
        let name = indicator_name(id);
        let list = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_LINE_LIST_{name}"))
            .style(
                StyleBuilder::new()
                    .width_full()
                    .flex_col()
                    .gap(LINE_GAP)
                    .build(),
            )
            .build(self.cx.tree_mut(), parent)?;
        let can_remove = can_remove_line(min_rows, lines.rows.len());
        for index in 0..lines.rows.len() {
            let input_key = InputKey {
                id,
                row: Some(index),
                key: row.key,
            };
            let color_key = ColorKey {
                id,
                row: Some(index),
                key: row.key,
            };
            let (Some(field), Some(color)) = (
                inputs::field_of(config, input_key),
                colors::field_of(config, color_key),
            ) else {
                continue;
            };
            self.line_row(id, index, field, color, can_remove, list)?;
        }
        self.add_line_row(id, list)?;
        Ok(())
    }

    /// One line of a line list: its period field, the picker that sets its colour, and the button
    /// that deletes it.
    fn line_row(
        &mut self,
        id: IndicatorId,
        index: usize,
        field: InputField,
        color: ColorField,
        can_remove: bool,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let name = indicator_name(id);
        let input_key = InputKey {
            id: field.id,
            row: field.row,
            key: field.key,
        };
        let border = self.theme.tokens().border_width;
        let holder = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_LINE_ROW_{name}_{index}"))
            .style(StyleBuilder::new().width_full().flex_col().build())
            .build(self.cx.tree_mut(), parent)?;
        let line_node = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .height(SETTING_ROW_HEIGHT)
                    .flex_row()
                    .items_center()
                    .gap(ROW_GAP_X)
                    .padding_sides(0.0, 0.0, 0.0, setting_inset(&self.theme))
                    .relative()
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .border(border, Color::TRANSPARENT)
                    .build(),
            )
            .bind_style(invalid_border(&self.theme, self.wiring.invalid, input_key))
            .build(self.cx.tree_mut(), holder)?;
        let field_node = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_LINE_FIELD_{name}_{index}"))
            .style(
                StyleBuilder::new()
                    .width(PERIOD_FIELD_WIDTH)
                    .flex_none()
                    .build(),
            )
            // A line has no visible label of its own; the group's name is the setting it edits,
            // which is what a reader needs to hear before the field's value.
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                label: Some(t_smol!(
                    self.cx.translate(),
                    "widget.candlestick.indicator.period"
                )),
                ..A11yOverride::default()
            })
            .build(self.cx.tree_mut(), line_node)?;
        let suffix = format_smolstr!("panel.line.{name}.{index}.period");
        let input = FieldInput::build(&mut *self.cx, field_node, &field, suffix.as_str());
        self.controls.push(Control::Input(input));
        NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_1().build())
            .build(self.cx.tree_mut(), line_node)?;
        let labels = color_labels(self.cx.translate());
        let color_holder = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_COLOR_{name}_{index}"))
            .style(StyleBuilder::new().flex_none().build())
            .build(self.cx.tree_mut(), line_node)?;
        let suffix = format_smolstr!("panel.color.{name}.{index}");
        let picker = FieldPicker::build(
            &mut *self.cx,
            color_holder,
            &color,
            &labels,
            self.wiring.edits,
            suffix.as_str(),
        )?;
        self.controls.push(Control::Picker(picker));
        if can_remove {
            self.delete_line(id, index, line_node)?;
        }
        self.invalid_hint(
            input_key,
            format_smolstr!("TEST_PANEL_INVALID_{name}_{index}"),
            holder,
        )?;
        Ok(())
    }

    /// The button that deletes one line.
    ///
    /// The last line of a list has no delete button at all: a drawn but disabled one would offer a
    /// control that can never act, and the configuration refuses the removal anyway.
    fn delete_line(
        &mut self,
        id: IndicatorId,
        index: usize,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let name = indicator_name(id);
        let label = t_smol!(
            self.cx.translate(),
            "widget.candlestick.indicator.remove_line"
        );
        let holder = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_DELETE_LINE_{name}_{index}"))
            .style(StyleBuilder::new().flex_none().build())
            .build(self.cx.tree_mut(), parent)?;
        let key = self
            .cx
            .unique_key(&format_smolstr!("panel.line.{name}.{index}.delete"));
        let edits = self.wiring.edits;
        let window = self.cx.window();
        let button = Button::new(self.cx.app_mut(), self.theme.clone(), key)
            .outline()
            .small()
            .icon(IconKind::Trash)
            .icon_size(ROW_ICON_SIZE)
            .a11y_label(label)
            .on_click(move |ctx| {
                ctx.write(&edits, Some(PanelEdit::RemoveLine { id, index }));
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(self.cx.app_mut(), window, holder)?;
        self.controls.push(Control::Button(button));
        Ok(())
    }

    /// The "+" row: one button under the last line of a list.
    fn add_line_row(&mut self, id: IndicatorId, parent: NodeId) -> Result<(), EngineError> {
        let name = indicator_name(id);
        let label = t_smol!(self.cx.translate(), "widget.candlestick.indicator.add_line");
        let holder = NodeBuilder::boxed()
            .id(format_smolstr!("TEST_PANEL_ADD_LINE_{name}"))
            .style(
                StyleBuilder::new()
                    .width_full()
                    .flex_row()
                    .padding_sides(0.0, 0.0, 0.0, setting_inset(&self.theme))
                    .build(),
            )
            .build(self.cx.tree_mut(), parent)?;
        let key = self
            .cx
            .unique_key(&format_smolstr!("panel.add_line.{name}"));
        let edits = self.wiring.edits;
        let window = self.cx.window();
        let button = Button::new(self.cx.app_mut(), self.theme.clone(), key)
            .outline()
            .small()
            .label(label.clone())
            .a11y_label(label)
            .on_click(move |ctx| {
                ctx.write(&edits, Some(PanelEdit::AddLine(id)));
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(self.cx.app_mut(), window, holder)?;
        self.controls.push(Control::Button(button));
        Ok(())
    }

    /// A row label: a text node whose sentence is resolved on the frame that draws it.
    fn label_text(
        &mut self,
        key: &'static str,
        id: SmolStr,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let translate = self.cx.translate().clone();
        let language = self.cx.language();
        let theme = self.theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id(id)
        .style(StyleBuilder::new().flex_1().min_width_0().build())
        .bind_text(move |read| {
            let _language = *read.read(&language);
            t_smol!(translate, key)
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(self.cx.tree_mut(), parent)
    }

    /// The hint under a field whose text is not a period it may take.
    ///
    /// The node is always built and carries no text while the field is valid: an empty text
    /// measures nothing, so the row is as tall as its controls, and the sentence grows it only
    /// while the user is looking at it.
    fn invalid_hint(
        &mut self,
        key: InputKey,
        id: SmolStr,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let translate = self.cx.translate().clone();
        let language = self.cx.language();
        let theme = self.theme.clone();
        let invalid = self.wiring.invalid;
        let inset = setting_inset(&self.theme) + HINT_INSET;
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id(id)
        .style(
            StyleBuilder::new()
                .padding_sides(0.0, 0.0, 0.0, inset)
                .build(),
        )
        .bind_text(move |read| {
            let _language = *read.read(&language);
            if *read.read(&invalid) == Some(key) {
                t_smol!(translate, "widget.candlestick.indicator.period_invalid")
            } else {
                SmolStr::default()
            }
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::StatusDangerBase)),
            ..StylePatch::default()
        })
        .build(self.cx.tree_mut(), parent)?;
        Ok(())
    }

    /// Registers the action that opens or folds one indicator's settings, and returns it.
    ///
    /// The action identity is minted from the pane's own scope, so two panes never share one, and
    /// a row rebuilt on a later frame reclaims the action it registered before.
    fn register_expand(&mut self, id: IndicatorId, suffix: &str) -> Action {
        let action = Action::Custom(format_smolstr!("{}.click", self.cx.unique_key(suffix)));
        let edits = self.wiring.edits;
        let window = self.cx.window();
        self.cx.app_mut().on_action(action.clone(), move |ctx| {
            ctx.write(&edits, Some(PanelEdit::ToggleExpanded(id)));
            let _ = ctx.app_mut().request_repaint(window);
        });
        self.actions.push(action.clone());
        action
    }

    /// The hover mirror of one card.
    fn hover_signal(&self, id: IndicatorId) -> Signal<bool> {
        self.hover
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .map(|(_, signal)| *signal)
            .expect("every catalogue indicator has a hover mirror")
    }

    /// Whether `id`'s settings are open.
    ///
    /// An indicator the list does not name is open: that is the same answer the constructor
    /// gives, so a catalogue entry the panel has not seen starts open rather than folded and
    /// unreachable.
    fn is_expanded(&self, id: IndicatorId) -> bool {
        self.expanded
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .is_none_or(|(_, open)| *open)
    }
}

/// The colours one card is painted in, given what it is and whether the pointer is on it.
///
/// A pure function so the three states the design asks for are one piece of logic a test can call:
/// a folded card under the pointer, a folded card at rest, and an open card (which draws no hover
/// at all). The hover lifts the surface to the control colour and strengthens the edge rather than
/// compositing a veil; the engine's paint has no blend step for the panel to ask for, and the
/// difference is recorded in the migration report.
pub(super) fn card_colors(theme: &Theme, folded: bool, hovered: bool) -> (Color, Color) {
    let background = theme.role(Role::SurfaceSunken);
    if !folded || !hovered {
        return (background, theme.role(Role::LineDivider));
    }
    (
        theme.role(Role::SurfaceControl),
        theme.role(Role::LineStrong),
    )
}

/// Whether a line list with `rows` lines may lose one of them.
///
/// A line list never goes below the minimum its catalogue row declares, and at that minimum the
/// row's delete button is not drawn at all.
pub(super) fn can_remove_line(min_rows: usize, rows: usize) -> bool {
    rows > min_rows
}

/// The icon that states whether an indicator's settings are open.
///
/// The one place the direction is decided: the caret points down while the settings are drawn and
/// right while they are folded, so the same control says both "this is open" and "this opens".
pub(super) fn caret_icon(expanded: bool) -> IconKind {
    if expanded {
        IconKind::CaretDown
    } else {
        IconKind::CaretRight
    }
}

/// The border of a row that holds a field in error, transparent while the field is valid.
///
/// The frame is painted over the row rather than declared as a layout border, so a row holds its
/// controls at the same inset whether or not the field is in error.
fn invalid_border(
    theme: &ThemeHandle,
    invalid: Signal<Option<InputKey>>,
    key: InputKey,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    let theme = theme.clone();
    let width = theme.tokens().border_width;
    move |read| {
        let color = if *read.read(&invalid) == Some(key) {
            theme.role(read, Role::StatusDangerBorder)
        } else {
            Color::TRANSPARENT
        };
        StylePatch {
            border: Some(Border { width, color }),
            ..StylePatch::default()
        }
    }
}

/// The width of the caret button, from the kit's small size step.
///
/// A small button is its icon plus its own horizontal paddings and its layout border; the caret
/// overrides the icon edge to 12 so the glyph keeps the size the old panel gave it.
fn caret_width(theme: &ThemeHandle) -> f32 {
    ROW_ICON_SIZE + 2.0 * UnifiedSize::Small.px() + 2.0 * theme.tokens().border_width
}

/// How far a settings row insets a control whose visible edge is its own box.
///
/// The node row puts the checkbox one caret and one gap in, and the square inside that checkbox a
/// further small-control padding. A label or a field has no such padding of its own, so it starts
/// at that many pixels to read as the same column.
fn setting_inset(theme: &ThemeHandle) -> f32 {
    caret_width(theme) + NODE_GAP + UnifiedSize::Small.px()
}

/// How far a settings row insets a checkbox, whose visible edge is its square.
///
/// A checkbox draws one small-control padding of its own before the square, so putting its box on
/// [`setting_inset`] would push its square past every label above it. It takes one padding less,
/// and the rows line up by what is drawn.
fn checkbox_inset(theme: &ThemeHandle) -> f32 {
    setting_inset(theme) - UnifiedSize::Small.px()
}

/// The heading key of one region of the chart.
fn section_key(pane: IndicatorPane) -> &'static str {
    match pane {
        IndicatorPane::Main => "widget.candlestick.indicator.main",
        IndicatorPane::Sub => "widget.candlestick.indicator.sub",
    }
}

/// The short name indicators are written under in test selectors and kit keys.
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

/// The name one setting is written under in a test selector or a kit key.
fn setting_name(key: SettingKey) -> &'static str {
    match key {
        SettingKey::Period => "PERIOD",
        SettingKey::ShortPeriod => "SHORT",
        SettingKey::LongPeriod => "LONG",
        SettingKey::SignalPeriod => "SIGNAL",
        SettingKey::Smoothing1 => "SMOOTHING1",
        SettingKey::Smoothing2 => "SMOOTHING2",
        SettingKey::Lines => "LINES",
        SettingKey::Dif => "DIF",
        SettingKey::Dea => "DEA",
        SettingKey::K => "K",
        SettingKey::D => "D",
        SettingKey::J => "J",
        SettingKey::DifColor => "DIF_COLOR",
        SettingKey::DeaColor => "DEA_COLOR",
        SettingKey::KColor => "K_COLOR",
        SettingKey::DColor => "D_COLOR",
        SettingKey::JColor => "J_COLOR",
    }
}

/// The short name one region of the chart is written under in a test selector.
fn pane_name(pane: IndicatorPane) -> &'static str {
    match pane {
        IndicatorPane::Main => "MAIN",
        IndicatorPane::Sub => "SUB",
    }
}

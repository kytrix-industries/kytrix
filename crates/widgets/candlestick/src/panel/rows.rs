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
use ui_kit::{
    kit::{
        button::Button,
        icon::IconKind,
        size::{UnifiedSizable, UnifiedSize},
        toggle::Toggle,
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, resolved::Theme, role::Role},
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

const NODE_ROW_HEIGHT: f32 = 28.0;
const SETTING_ROW_HEIGHT: f32 = 32.0;
const NODE_GAP: f32 = 4.0;
const ROW_GAP_X: f32 = 8.0;
const LINE_GAP: f32 = 4.0;
pub(super) const PERIOD_FIELD_WIDTH: f32 = 60.0;
const ROW_ICON_SIZE: f32 = 12.0;
const CARD_PADDING: f32 = 8.0;
const CARD_RADIUS: f32 = 4.0;
const HINT_INSET: f32 = 4.0;

#[derive(Clone, Copy)]
pub(super) struct Wiring {
    pub edits: Signal<Option<PanelEdit>>,
    pub invalid: Signal<Option<InputKey>>,
}

pub(super) struct BuiltRows {
    pub controls: Vec<Control>,
    pub actions: Vec<Action>,
    pub cards: Vec<(IndicatorId, NodeId)>,
}

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

    fn hover_signal(&self, id: IndicatorId) -> Signal<bool> {
        self.hover
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .map(|(_, signal)| *signal)
            .expect("every catalogue indicator has a hover mirror")
    }

    fn is_expanded(&self, id: IndicatorId) -> bool {
        self.expanded
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .is_none_or(|(_, open)| *open)
    }
}

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

pub(super) fn can_remove_line(min_rows: usize, rows: usize) -> bool {
    rows > min_rows
}

pub(super) fn caret_icon(expanded: bool) -> IconKind {
    if expanded {
        IconKind::CaretDown
    } else {
        IconKind::CaretRight
    }
}

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

fn caret_width(theme: &ThemeHandle) -> f32 {
    ROW_ICON_SIZE + 2.0 * UnifiedSize::Small.px() + 2.0 * theme.tokens().border_width
}

fn setting_inset(theme: &ThemeHandle) -> f32 {
    caret_width(theme) + NODE_GAP + UnifiedSize::Small.px()
}

fn checkbox_inset(theme: &ThemeHandle) -> f32 {
    setting_inset(theme) - UnifiedSize::Small.px()
}

fn section_key(pane: IndicatorPane) -> &'static str {
    match pane {
        IndicatorPane::Main => "widget.candlestick.indicator.main",
        IndicatorPane::Sub => "widget.candlestick.indicator.sub",
    }
}

pub(super) fn indicator_name(id: IndicatorId) -> &'static str {
    match id {
        IndicatorId::Ma => "MA",
        IndicatorId::Ema => "EMA",
        IndicatorId::Vol => "VOL",
        IndicatorId::Macd => "MACD",
        IndicatorId::Rsi => "RSI",
        IndicatorId::Kdj => "KDJ",
    }
}

pub(super) fn setting_name(key: SettingKey) -> &'static str {
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

fn pane_name(pane: IndicatorPane) -> &'static str {
    match pane {
        IndicatorPane::Main => "MAIN",
        IndicatorPane::Sub => "SUB",
    }
}

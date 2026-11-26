use std::sync::Arc;

use smol_str::{SmolStr, format_smolstr};
use ui_core::node::signal::{Signal, SignalScope};
use ui_kit::{
    kit::{
        input::{self as kit_input, InputNode, InputSpec, InputState},
        size::UnifiedSize,
    },
    theme::handle::ThemeHandle,
};
use widget::cx::WidgetCx;

use super::view::PanelEdit;
use crate::indicator::{
    CandlestickIndicatorConfig, IndicatorId, PERIOD_MAX, PERIOD_MIN, SettingControl, SettingKey,
    definition_of,
};

const INDICATOR_FIELD_CAPACITY: usize = 16;

pub(super) struct InputField {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
    pub period: u32,
    pub min: u32,
    pub max: u32,
}

pub(super) fn input_fields(config: &CandlestickIndicatorConfig) -> Vec<InputField> {
    let mut fields = Vec::with_capacity(INDICATOR_FIELD_CAPACITY);
    for entry in &config.entries {
        let definition = definition_of(entry.id);
        for row in definition.rows {
            match row.control {
                SettingControl::LineList { .. } => {
                    let Some(lines) = entry.settings.lines() else {
                        continue;
                    };
                    for (index, line) in lines.rows.iter().enumerate() {
                        fields.push(InputField {
                            id: entry.id,
                            row: Some(index),
                            key: row.key,
                            period: line.period,
                            min: PERIOD_MIN,
                            max: PERIOD_MAX,
                        });
                    }
                }
                SettingControl::Period { min, max } => {
                    let Some(period) = entry.settings.period(row.key) else {
                        continue;
                    };
                    fields.push(InputField {
                        id: entry.id,
                        row: None,
                        key: row.key,
                        period,
                        min,
                        max,
                    });
                }
                SettingControl::Switch => {}
            }
        }
    }
    fields
}

pub(super) fn field_of(config: &CandlestickIndicatorConfig, key: InputKey) -> Option<InputField> {
    input_fields(config)
        .into_iter()
        .find(|field| field.id == key.id && field.row == key.row && field.key == key.key)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InputKey {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
}

pub(super) fn parse_period(text: &str, min: u32, max: u32) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|period| (min..=max).contains(period))
}

pub(super) struct FieldInput {
    key: InputKey,
    stored: u32,
    min: u32,
    max: u32,
    node: InputNode,
    scope: SignalScope,
    last_text: SmolStr,
    last_focused: bool,
}

impl FieldInput {
    pub(super) fn build(
        cx: &mut WidgetCx<'_>,
        parent: ui_core::node::id::NodeId,
        field: &InputField,
        suffix: &str,
    ) -> Result<Self, ui_core::error::EngineError> {
        let theme: ThemeHandle = cx.theme().clone();
        let name = cx.unique_key(suffix);
        let text = period_text(field.period);
        let scope = cx.app_mut().signal_scope();
        let state = cx
            .app_mut()
            .signal_scoped(scope, InputState::new(text.as_str()));
        let placeholder = cx.app_mut().signal_scoped(scope, SmolStr::new_static(""));
        let spec = Arc::new(InputSpec {
            name,
            label: None,
            placeholder: SmolStr::new_static(""),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: UnifiedSize::Small,
            submit: None,
        });
        let node = kit_input::build(cx.tree_mut(), parent, spec, &theme, &state, &placeholder);
        node.install(cx.app_mut())?;
        Ok(Self {
            key: InputKey {
                id: field.id,
                row: field.row,
                key: field.key,
            },
            stored: field.period,
            min: field.min,
            max: field.max,
            node,
            scope,
            last_text: text,
            last_focused: false,
        })
    }

    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        invalid: Signal<Option<InputKey>>,
    ) -> Option<PanelEdit> {
        self.node.sync(cx.app_mut());
        let (text, focused) = {
            let state = cx.read(&self.node.state);
            (SmolStr::new(state.text()), state.is_focused())
        };
        if text != self.last_text {
            self.last_text = text;
            self.note_validity(cx, invalid);
        }
        let mut edit = None;
        if self.last_focused && !focused {
            match parse_period(self.last_text.as_str(), self.min, self.max) {
                Some(period) => {
                    self.clear_invalid(cx, invalid);
                    if period != self.stored {
                        edit = Some(PanelEdit::SetPeriod {
                            key: self.key,
                            period,
                        });
                    }
                }
                None => {
                    self.restore(cx);
                    self.clear_invalid(cx, invalid);
                }
            }
        }
        self.last_focused = focused;
        edit
    }

    pub(super) fn release(self, cx: &mut WidgetCx<'_>) {
        let _ = self.node.release(cx.app_mut());
        cx.app_mut().release_scope(self.scope);
    }

    fn note_validity(&mut self, cx: &mut WidgetCx<'_>, invalid: Signal<Option<InputKey>>) {
        if parse_period(self.last_text.as_str(), self.min, self.max).is_some() {
            self.clear_invalid(cx, invalid);
            return;
        }
        let current = *cx.read(&invalid);
        if current != Some(self.key) {
            cx.write(&invalid, Some(self.key));
        }
    }

    fn clear_invalid(&mut self, cx: &mut WidgetCx<'_>, invalid: Signal<Option<InputKey>>) {
        let current = *cx.read(&invalid);
        if current == Some(self.key) {
            cx.write(&invalid, None);
        }
    }

    fn restore(&mut self, cx: &mut WidgetCx<'_>) {
        let text = period_text(self.stored);
        self.node.update_value(cx.app_mut(), text.as_str());
        self.last_text = text;
    }
}

fn period_text(period: u32) -> SmolStr {
    format_smolstr!("{period}")
}

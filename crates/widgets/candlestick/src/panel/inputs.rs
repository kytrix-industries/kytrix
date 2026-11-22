//! The period fields the panel's numeric settings are typed into.
//!
//! One field per editable period: every line of a line list, and every named period row of an
//! indicator. A field owns a [`kit::input`](base_ui::kit::input) node and the signals it edits;
//! the row builder in [`super::rows`] owns the label and the row the field is placed in.
//!
//! The kit input has no change / blur callbacks: both answers are read off the state signal. The
//! field therefore remembers the text and the focus it saw on the previous sync — a text change
//! is typing, a focus drop is the caret leaving — which is the same pair of answers the old panel
//! took from `InputStateEvent::{Change, Blur}`. The engine mirrors focus into the state signal in
//! `InputNode::sync`, so a commit lands on the first frame after the focus moved rather than
//! inside the click handler; the field is not rebuilt while it is being typed into, so its caret
//! and its text survive.
//!
//! Typing never writes the configuration: a half-typed number is a value the user has not given
//! yet, and only a text the caret left that parses into the field's range is committed. Anything
//! else puts the stored value back into the field, which is where the old panel's deferred reset
//! came from.

use std::sync::Arc;

use base_ui::{
    kit::{
        input::{self as kit_input, InputNode, InputSpec, InputState},
        size::UnifiedSize,
    },
    theme::handle::ThemeHandle,
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    event::Event,
    node::signal::{Signal, SignalScope},
};
use widget::cx::WidgetCx;

use super::view::PanelEdit;
use crate::indicator::{
    CandlestickIndicatorConfig, IndicatorId, PERIOD_MAX, PERIOD_MIN, SettingControl, SettingKey,
    definition_of,
};

/// Roughly how many fields the default configuration has, so that listing them on every rebuild
/// does not grow a vector.
///
/// MA's three lines, EMA's one, RSI's three, MACD's three periods and KDJ's three are thirteen;
/// sixteen leaves room for a few more lines without turning this into a limit.
const INDICATOR_FIELD_CAPACITY: usize = 16;

/// One editable period field: which indicator and line it belongs to, the value it has to show,
/// and the inclusive range its text may fall in.
pub(super) struct InputField {
    pub id: IndicatorId,
    /// The line of a line list; `None` for a named `Period` row.
    pub row: Option<usize>,
    pub key: SettingKey,
    pub period: u32,
    pub min: u32,
    pub max: u32,
}

/// Every period the panel offers a field for, in the order they are drawn.
///
/// Read from the configuration rather than stored: the configuration is the one owner of what the
/// fields show, and a second copy of the numbers here would be a second thing to keep in step.
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

/// The field `key` names, if the configuration still holds it.
///
/// The row builder asks for the value it has to draw rather than reading the settings value
/// itself: which rows are editable periods, and what range each takes, is stated once in
/// [`input_fields`].
pub(super) fn field_of(config: &CandlestickIndicatorConfig, key: InputKey) -> Option<InputField> {
    input_fields(config)
        .into_iter()
        .find(|field| field.id == key.id && field.row == key.row && field.key == key.key)
}

/// Which field an input event, or the error mark on a row, belongs to.
///
/// It is an identity rather than a position in a vector: the rows are rebuilt whenever the
/// configuration or the language moves, so what pairs a field with the value it edits has to
/// survive a rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InputKey {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
}

/// Reads a period out of the text a field holds.
///
/// `trim` because a field the user leaves with a trailing space holds a number they typed rather
/// than a different answer; the range is inclusive on both ends, which is what the hint's sentence
/// names.
pub(super) fn parse_period(text: &str, min: u32, max: u32) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|period| (min..=max).contains(period))
}

/// One period field's kit input, its edit state, and the scope its signals belong to.
pub(super) struct FieldInput {
    key: InputKey,
    /// The value the field puts back when its text is not a period it may take.
    stored: u32,
    min: u32,
    max: u32,
    node: InputNode,
    /// The scope the input's state and placeholder signals were minted in; released with the
    /// field so a configuration change that drops a row does not leak a slot.
    scope: SignalScope,
    /// The text the previous sync saw, and whether the field held the focus then.
    last_text: SmolStr,
    last_focused: bool,
}

impl FieldInput {
    /// Builds `field`'s kit input under `parent` and installs its click / drag actions.
    ///
    /// `suffix` is the panel-local control position; the key the input registers its actions
    /// under is derived from it, so two panes never share one and a rebuilt row reclaims its own.
    pub(super) fn build(
        cx: &mut WidgetCx<'_>,
        parent: ui_core::node::id::NodeId,
        field: &InputField,
        suffix: &str,
    ) -> Self {
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
            placeholder: SmolStr::new_static(""),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: UnifiedSize::Small,
            submit: None,
        });
        let node = kit_input::build(cx.tree_mut(), parent, spec, &theme, &state, &placeholder);
        node.install(cx.app_mut());
        Self {
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
        }
    }

    /// Mirrors focus into the input, then answers what the user did with the text since the last
    /// sync: a caret that left the field commits a period or puts the stored one back.
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

    /// Routes one platform event to the input (only the focused field consumes it).
    pub(super) fn dispatch(&self, cx: &mut WidgetCx<'_>, event: &Event) -> bool {
        self.node.dispatch(cx.app_mut(), event)
    }

    /// Releases the input's action handlers and the scope its signals were minted in.
    ///
    /// The row it was built into is removed by the panel before this runs.
    pub(super) fn release(self, cx: &mut WidgetCx<'_>) {
        let _ = self.node.release(cx.app_mut());
        cx.app_mut().release_scope(self.scope);
    }

    /// Marks the row in error while the text is not a period the field may take.
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

    /// Takes the error mark down if it is this field's.
    fn clear_invalid(&mut self, cx: &mut WidgetCx<'_>, invalid: Signal<Option<InputKey>>) {
        let current = *cx.read(&invalid);
        if current == Some(self.key) {
            cx.write(&invalid, None);
        }
    }

    /// Puts the stored value back into the field.
    ///
    /// The kit input writes programmatically without the user-edit restrictions (`update_value`),
    /// so this needs no window and runs on the frame the caret leaving was seen.
    fn restore(&mut self, cx: &mut WidgetCx<'_>) {
        let text = period_text(self.stored);
        self.node.update_value(cx.app_mut(), text.as_str());
        self.last_text = text;
    }
}

/// The text one period is typed as.
fn period_text(period: u32) -> SmolStr {
    format_smolstr!("{period}")
}

//! The keyed table of `InputState` entities the panel's parameter fields edit.
//!
//! One text field per editable period: every line of a line list, and every
//! `Period` row of an indicator that declares named periods. The entity is built
//! the first time the field is drawn and reused afterwards — an input rebuilt on
//! every frame is an input the user is typing into that loses what they typed —
//! and it is rebuilt only when the value it was built around moved, which is what
//! keeps a field from showing a number the configuration no longer holds.

use base_ui::input::{InputState, InputStateBuilder, InputStateEvent};
use gpui_kit::{AppContext, Context, ElementId, Entity, SharedString, Subscription, Window};
use smol_str::format_smolstr;

use super::view::IndicatorPanel;
use crate::indicator::{
    CandlestickIndicatorConfig, IndicatorId, PERIOD_MAX, PERIOD_MIN, SettingControl, SettingKey,
    definition_of,
};

/// Roughly how many fields the default configuration has, so that listing them
/// every frame does not grow a vector.
///
/// MA's three lines, EMA's one, RSI's three, MACD's three periods and KDJ's
/// three are thirteen; sixteen leaves room for a few more lines without turning
/// this into a limit. A configuration with more lines is not bounded, so this is
/// a starting capacity rather than a cap.
const INDICATOR_FIELD_CAPACITY: usize = 16;

/// One editable period field: which indicator and line it belongs to, the value
/// it has to show, and the inclusive range its text may fall in.
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
/// Read from the configuration rather than stored: the configuration is the one
/// owner of what the fields show, and a second copy of the numbers here would be
/// a second thing to keep in step with what was published.
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
pub(super) fn field_of(config: &CandlestickIndicatorConfig, key: InputKey) -> Option<InputField> {
    input_fields(config)
        .into_iter()
        .find(|field| field.id == key.id && field.row == key.row && field.key == key.key)
}

/// Which field an input event, or the error mark on a row, belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InputKey {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
}

/// One field's text entity, and the connection that makes it edit the config.
pub(super) struct LineInput {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
    /// The value the field was built around. A field whose stored value moved
    /// away from this is rebuilt, because the user is then looking at the wrong
    /// number.
    pub built_from: u32,
    /// The number that tells this field's elements apart from its siblings'.
    ///
    /// Handed out by the panel and never reused, so a field that is drawn again
    /// after its line was deleted is a different element: reusing the deleted
    /// one would resurrect the text it held.
    pub slot: u64,
    pub entity: Entity<InputState>,
    /// Held because the subscriptions are what commit the field: dropping them
    /// would leave a field that is typed into and never written.
    _events: Vec<Subscription>,
}

impl LineInput {
    /// Builds `field`'s entity and connects it to `panel`.
    pub(super) fn connect(
        field: &InputField,
        slot: u64,
        window: &mut Window,
        cx: &mut Context<IndicatorPanel>,
    ) -> Self {
        let entity = cx.new(|cx| {
            InputStateBuilder::new(ElementId::NamedInteger(
                SharedString::new_static("CandlestickIndicatorPeriodInput"),
                slot,
            ))
            .default_value(format_smolstr!("{}", field.period).as_str())
            .build(window, cx)
        });
        let key = InputKey {
            id: field.id,
            row: field.row,
            key: field.key,
        };
        // Two subscriptions rather than one with a `match`: typing and leaving
        // the field are two different answers to the text, and each entry point
        // stays the only thing it does.
        let mut events = Vec::with_capacity(2);
        events.push(
            cx.subscribe(&entity, move |panel, _, event: &InputStateEvent, cx| {
                if let InputStateEvent::Change(text) = event {
                    panel.on_input_change(key, text, cx);
                }
            }),
        );
        events.push(
            cx.subscribe(&entity, move |panel, _, event: &InputStateEvent, cx| {
                if let InputStateEvent::Blur(text) = event {
                    panel.on_input_blur(key, text, cx);
                }
            }),
        );
        Self {
            id: field.id,
            row: field.row,
            key: field.key,
            built_from: field.period,
            slot,
            entity,
            _events: events,
        }
    }
}

//! The keyed table of colour pickers the panel's colour settings edit.
//!
//! One picker per configurable colour: every line of a line list, and one for
//! each switch row that controls a line whose colour the user may choose. Every
//! picker is built by the panel's first sync — including fields whose indicator is
//! folded, because the field is still part of the configuration — and reused
//! afterwards; a picker rebuilt on every frame would lose the tab the user left it
//! on. A picker is dropped with the field it edits, so a line added later does not
//! come back with a colour nobody stored.

use base_ui::{
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerLabels},
    size::UnifiedSize,
};
use gpui_kit::{App, AppContext, Context, Entity, Hsla, Subscription, Window};
use i18n::t;

use super::view::IndicatorPanel;
use crate::indicator::{
    CandlestickIndicatorConfig, IndicatorId, LineColor, SettingControl, SettingKey, definition_of,
};

/// How many colour fields the default configuration has, so that listing them
/// every frame does not grow a vector.
///
/// MA's three lines, EMA's one, RSI's three, MACD's two and KDJ's three are
/// twelve. A configuration with more lines is not bounded, so this is a starting
/// capacity rather than a cap.
const COLOR_FIELD_CAPACITY: usize = 12;

/// One configurable colour: which indicator and line it belongs to, and the
/// exact colour the configuration holds for it.
pub(super) struct ColorField {
    pub id: IndicatorId,
    /// The line of a line list; `None` for a switch row's colour.
    pub row: Option<usize>,
    pub key: SettingKey,
    pub color: LineColor,
}

impl ColorField {
    /// The identity this field's picker is keyed by.
    pub(super) fn key(&self) -> ColorKey {
        ColorKey {
            id: self.id,
            row: self.row,
            key: self.key,
        }
    }
}

/// Which colour field a picker, a change or a commit belongs to.
///
/// It is the same shape as [`super::inputs::InputKey`] and serves the same
/// purpose: the panel is redrawn every frame, so what pairs a control with the
/// value it edits has to be an identity, not a position in a vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ColorKey {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
}

/// Every colour the panel offers a picker for, in the order they are drawn.
///
/// Read from the configuration rather than stored: the configuration is the one
/// owner of what the pickers show. A switch row contributes its colour through
/// [`SettingKey`], and a line list contributes one field per line; periods have
/// no colour, and a value whose shape does not match its catalogue row
/// contributes nothing.
pub(super) fn color_fields(config: &CandlestickIndicatorConfig) -> Vec<ColorField> {
    let mut fields = Vec::with_capacity(COLOR_FIELD_CAPACITY);
    for entry in &config.entries {
        let definition = definition_of(entry.id);
        for row in definition.rows {
            match row.control {
                SettingControl::LineList { .. } => {
                    let Some(lines) = entry.settings.lines() else {
                        continue;
                    };
                    for (index, line) in lines.rows.iter().enumerate() {
                        fields.push(ColorField {
                            id: entry.id,
                            row: Some(index),
                            key: row.key,
                            color: line.color,
                        });
                    }
                }
                SettingControl::Period { .. } => {}
                SettingControl::Switch => {
                    let Some(key) = row.color_key else {
                        continue;
                    };
                    let Some(color) = entry.settings.color(key) else {
                        continue;
                    };
                    fields.push(ColorField {
                        id: entry.id,
                        row: None,
                        key,
                        color,
                    });
                }
            }
        }
    }
    fields
}

/// The panel's text, in the language of the frame being drawn.
///
/// Built every sync so a language switch localizes the popup too: the picker is
/// a `base_ui` view and cannot translate for itself. `set_labels` is a no-op when
/// the labels are equal, so handing the same ones over each frame costs nothing.
pub(super) fn color_labels(cx: &App) -> ColorPickerLabels {
    ColorPickerLabels {
        palette: t!(cx, "common.color.palette"),
        hsla: t!(cx, "common.color.hsla"),
        hue: t!(cx, "common.color.hue"),
        saturation: t!(cx, "common.color.saturation"),
        lightness: t!(cx, "common.color.lightness"),
        alpha: t!(cx, "common.color.alpha"),
    }
}

/// One field's picker, and the connection that makes a user's choice reach the
/// configuration.
pub(super) struct LinePicker {
    key: ColorKey,
    /// The number that tells this picker's tooltip apart from its siblings'.
    ///
    /// Handed out by the panel and never reused, so a picker drawn again after
    /// its line was deleted is a different element.
    slot: u64,
    entity: Entity<ColorPicker>,
    /// Held because the subscription is what stores the colour: dropping it would
    /// leave a picker that changes what is drawn and never what is saved.
    _events: Subscription,
}

impl LinePicker {
    /// The colour field this picker edits.
    pub(super) fn key(&self) -> ColorKey {
        self.key
    }

    /// The number that distinguishes this picker from its siblings.
    pub(super) fn slot(&self) -> u64 {
        self.slot
    }

    /// The picker view, which the row draws.
    pub(super) fn entity(&self) -> &Entity<ColorPicker> {
        &self.entity
    }

    /// Builds `field`'s picker and connects it to `panel`.
    ///
    /// A change is handed over as a preview, so the chart and the panel follow
    /// the picker while the user is still choosing; the commit is handed over
    /// only when the popup closes, which is what makes one interaction publish
    /// once instead of once per slider tick.
    pub(super) fn connect(
        field: &ColorField,
        slot: u64,
        labels: &ColorPickerLabels,
        window: &mut Window,
        cx: &mut Context<IndicatorPanel>,
    ) -> Self {
        let entity =
            cx.new(|cx| ColorPicker::new(Hsla::from(field.color), UnifiedSize::Small, window, cx));
        entity.update(cx, |picker, cx| picker.set_labels(labels.clone(), cx));
        let key = field.key();
        let events = cx.subscribe(
            &entity,
            move |panel, _, event: &ColorPickerEvent, cx| match event {
                ColorPickerEvent::ColorChanged(color) => panel.preview_color(key, *color, cx),
                ColorPickerEvent::ColorCommitted(color) => panel.commit_color(key, *color, cx),
            },
        );
        Self {
            key,
            slot,
            entity,
            _events: events,
        }
    }
}

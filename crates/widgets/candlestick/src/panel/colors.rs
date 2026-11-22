//! The colour wells the panel's colours are chosen from.
//!
//! One picker per configurable colour: every line of a line list, and one for each switch row that
//! controls a line whose colour the user may choose. The list is read from the configuration, so
//! it is the configuration that decides which colours exist; a line added later gets its picker
//! when the rows are rebuilt, and a line that was removed takes its picker with it.
//!
//! The kit picker reports a colour once per interaction — when the user confirms it in the
//! panel — and that report is the edit the panel hands to the widget. The picker's own state (the
//! tab it is on, the caret in its hex field) belongs to the user and is never overwritten from the
//! configuration while the rows stay built: the stored colour only changes the picker when the
//! configuration moved under it, which rebuilds the rows.

use base_ui::{
    kit::{
        color_picker::{ColorPicker, ColorPickerLabels},
        size::UnifiedSize,
    },
    theme::handle::ThemeHandle,
};
use i18n::{t_smol, translate::Translate};
use ui_core::{
    error::EngineError,
    event::Event,
    node::{id::NodeId, signal::Signal},
    paint::color::Color,
    window::WindowId,
};
use widget::cx::WidgetCx;

use super::view::PanelEdit;
use crate::indicator::{
    CandlestickIndicatorConfig, IndicatorId, LineColor, SettingControl, SettingKey, definition_of,
};

/// How many colour fields the default configuration has, so that listing them on every rebuild
/// does not grow a vector.
///
/// MA's three lines, EMA's one, RSI's three, MACD's two and KDJ's three are twelve. A
/// configuration with more lines is not bounded, so this is a starting capacity rather than a cap.
const COLOR_FIELD_CAPACITY: usize = 12;

/// One configurable colour: which indicator and line it belongs to, and the exact colour the
/// configuration holds for it.
pub(super) struct ColorField {
    pub id: IndicatorId,
    /// The line of a line list; `None` for a switch row's colour.
    pub row: Option<usize>,
    pub key: SettingKey,
    pub color: LineColor,
}

impl ColorField {
    /// The identity this field's picker and its edit are keyed by.
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
/// The same shape as [`super::inputs::InputKey`] and for the same reason: the rows are rebuilt
/// when the configuration moves, so a control is paired with the value it edits by identity, not
/// by its position in the built tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ColorKey {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
}

/// Every colour the panel offers a picker for, in the order they are drawn.
///
/// A switch row contributes its colour through [`SettingKey`], and a line list contributes one
/// field per line; periods have no colour, and a value whose shape does not match its catalogue
/// row contributes nothing.
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

/// The colour field `key` names, if the configuration still holds it.
///
/// The row builder asks for the colour it has to draw rather than reading the settings value
/// itself: which colours are configurable is stated once in [`color_fields`].
pub(super) fn field_of(config: &CandlestickIndicatorConfig, key: ColorKey) -> Option<ColorField> {
    color_fields(config)
        .into_iter()
        .find(|field| field.key() == key)
}

/// The picker's text, in the language of the frame that builds the rows.
///
/// The kit picker cannot translate for itself, and its copy is build-time (AGENTS §15.1): a
/// language switch rebuilds the rows, which hands the picker the new labels.
pub(super) fn color_labels(translate: &Translate) -> ColorPickerLabels {
    ColorPickerLabels {
        panel: t_smol!(translate, "widget.candlestick.indicator.line_color"),
        palette: t_smol!(translate, "common.color.palette"),
        hsla: t_smol!(translate, "common.color.hsla"),
        hue: t_smol!(translate, "common.color.hue"),
        saturation: t_smol!(translate, "common.color.saturation"),
        lightness: t_smol!(translate, "common.color.lightness"),
        alpha: t_smol!(translate, "common.color.alpha"),
        confirm: t_smol!(translate, "common.confirm"),
    }
}

/// One colour field's kit picker.
pub(super) struct FieldPicker {
    picker: ColorPicker,
}

impl FieldPicker {
    /// Builds `field`'s picker under `parent` and connects it to the panel's edits.
    ///
    /// `suffix` is the panel-local control position; the key the picker derives its actions from
    /// is minted from it, so two panes never collide and a rebuilt row reclaims its own key.
    pub(super) fn build(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        field: &ColorField,
        labels: &ColorPickerLabels,
        edits: Signal<Option<PanelEdit>>,
        suffix: &str,
    ) -> Result<Self, EngineError> {
        let theme: ThemeHandle = cx.theme().clone();
        let layer = cx.layer().clone();
        let router = cx.animation().clone();
        let key = cx.unique_key(suffix);
        let color = Color::from(field.color);
        let picker = ColorPicker::new(
            cx.app_mut(),
            theme,
            layer,
            router,
            key,
            color,
            UnifiedSize::Small,
        );
        picker.set_labels(cx.app_mut(), labels.clone());
        let color_key = field.key();
        let window: WindowId = cx.window();
        picker.on_change(move |app, color| {
            app.write(
                &edits,
                Some(PanelEdit::SetColor {
                    key: color_key,
                    color,
                }),
            );
            let _ = app.request_repaint(window);
        });
        picker.build(cx.app_mut(), window, parent)?;
        Ok(Self { picker })
    }

    /// Frame-driven sync: overlay state, trigger hover, the panel's own inputs.
    pub(super) fn sync(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.picker.sync(cx.app_mut())?;
        Ok(())
    }

    /// Routes one platform event to the picker (its hex field owns it while the panel is open).
    pub(super) fn dispatch(&self, cx: &mut WidgetCx<'_>, event: &Event) -> bool {
        self.picker.dispatch(cx.app_mut(), event)
    }

    /// Releases the picker's action handlers, overlay and signal scope.
    pub(super) fn release(self, cx: &mut WidgetCx<'_>) {
        let _ = self.picker.release(cx.app_mut());
    }
}

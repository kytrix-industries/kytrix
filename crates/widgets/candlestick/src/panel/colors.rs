use i18n::{t_smol, translate::Translate};
use ui_core::{
    error::EngineError,
    node::{id::NodeId, signal::Signal},
    paint::color::Color,
    window::WindowId,
};
use ui_kit::{
    kit::{
        color_picker::{ColorPicker, ColorPickerLabels},
        size::UnifiedSize,
    },
    theme::handle::ThemeHandle,
};
use widget::cx::WidgetCx;

use super::view::PanelEdit;
use crate::indicator::{
    CandlestickIndicatorConfig, IndicatorId, LineColor, SettingControl, SettingKey, definition_of,
};

const COLOR_FIELD_CAPACITY: usize = 12;

pub(super) struct ColorField {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
    pub color: LineColor,
}

impl ColorField {
    pub(super) fn key(&self) -> ColorKey {
        ColorKey {
            id: self.id,
            row: self.row,
            key: self.key,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ColorKey {
    pub id: IndicatorId,
    pub row: Option<usize>,
    pub key: SettingKey,
}

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

pub(super) fn field_of(config: &CandlestickIndicatorConfig, key: ColorKey) -> Option<ColorField> {
    color_fields(config)
        .into_iter()
        .find(|field| field.key() == key)
}

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

pub(super) struct FieldPicker {
    picker: ColorPicker,
}

impl FieldPicker {
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

    pub(super) fn sync(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.picker.sync(cx.app_mut())?;
        Ok(())
    }

    pub(super) fn release(self, cx: &mut WidgetCx<'_>) {
        let _ = self.picker.release(cx.app_mut());
    }
}

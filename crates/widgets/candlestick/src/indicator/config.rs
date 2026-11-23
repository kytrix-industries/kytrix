use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};
use ui_core::paint::color::{Color, ColorResolver};

use super::{
    INDICATOR_CATALOG, IndicatorId, IndicatorPane, SettingKey, default_line_color, definition_of,
};

pub(crate) const PERIOD_MIN: u32 = 1;

pub(crate) const PERIOD_MAX: u32 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct LineColor {
    pub h: f32,
    pub s: f32,
    pub l: f32,
    pub a: f32,
}

impl From<Color> for LineColor {
    fn from(color: Color) -> Self {
        let [r, g, b, a] = srgb_components(color);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let lightness = (max + min) / 2.0;
        let delta = max - min;
        if delta <= f32::EPSILON {
            return Self {
                h: 0.0,
                s: 0.0,
                l: lightness,
                a,
            };
        }
        let saturation = if lightness > 0.5 {
            delta / (2.0 - max - min)
        } else {
            delta / (max + min)
        };
        let hue = if (max - r).abs() <= f32::EPSILON {
            (g - b) / delta + if g < b { 6.0 } else { 0.0 }
        } else if (max - g).abs() <= f32::EPSILON {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        };
        Self {
            h: hue / 6.0,
            s: saturation,
            l: lightness,
            a,
        }
    }
}

impl From<LineColor> for Color {
    fn from(color: LineColor) -> Self {
        let hue = color.h.rem_euclid(1.0) * 6.0;
        let saturation = color.s.clamp(0.0, 1.0);
        let lightness = color.l.clamp(0.0, 1.0);
        let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
        let second = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
        let (red, green, blue) = match hue as u32 {
            0 => (chroma, second, 0.0),
            1 => (second, chroma, 0.0),
            2 => (0.0, chroma, second),
            3 => (0.0, second, chroma),
            4 => (second, 0.0, chroma),
            _ => (chroma, 0.0, second),
        };
        let base = lightness - chroma / 2.0;
        Self::srgb(
            red + base,
            green + base,
            blue + base,
            color.a.clamp(0.0, 1.0),
        )
    }
}

fn srgb_components(color: Color) -> [f32; 4] {
    if let Color::Srgb { r, g, b, alpha } = color {
        return [r, g, b, alpha];
    }
    let resolved = ColorResolver.resolve(color).components;
    let alpha = resolved[3];
    if alpha <= 0.0 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    [
        linear_to_srgb(resolved[0] / alpha),
        linear_to_srgb(resolved[1] / alpha),
        linear_to_srgb(resolved[2] / alpha),
        alpha,
    ]
}

fn linear_to_srgb(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

impl<'de> Deserialize<'de> for LineColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct LineColorMap {
            h: f32,
            s: f32,
            l: f32,
            a: f32,
        }

        struct LineColorVisitor;

        impl<'de> Visitor<'de> for LineColorVisitor {
            type Value = LineColor;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a map with `h`, `s`, `l` and `a` components")
            }

            fn visit_map<A>(self, map: A) -> Result<LineColor, A::Error>
            where
                A: MapAccess<'de>,
            {
                let map = LineColorMap::deserialize(MapAccessDeserializer::new(map))?;
                Ok(LineColor {
                    h: map.h,
                    s: map.s,
                    l: map.l,
                    a: map.a,
                })
            }
        }

        deserializer.deserialize_map(LineColorVisitor)
    }
}

impl LineColor {
    pub(crate) fn normalized(self, fallback: Self) -> Self {
        if ![self.h, self.s, self.l, self.a]
            .iter()
            .all(|component| component.is_finite())
        {
            return fallback;
        }
        Self {
            h: if (0.0..=1.0).contains(&self.h) {
                self.h
            } else {
                self.h.rem_euclid(1.0)
            },
            s: self.s.clamp(0.0, 1.0),
            l: self.l.clamp(0.0, 1.0),
            a: self.a.clamp(0.0, 1.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndicatorLineConfig {
    pub period: u32,
    pub color: LineColor,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct LineListConfig {
    pub rows: Vec<IndicatorLineConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub(crate) enum IndicatorSettings {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "lines")]
    Lines(LineListConfig),
    #[serde(rename = "macd")]
    Macd {
        #[serde(rename = "short_period")]
        short_period: u32,
        #[serde(rename = "long_period")]
        long_period: u32,
        #[serde(rename = "signal_period")]
        signal_period: u32,
        #[serde(rename = "dif")]
        dif: bool,
        #[serde(rename = "dea")]
        dea: bool,
        #[serde(rename = "dif_color")]
        dif_color: LineColor,
        #[serde(rename = "dea_color")]
        dea_color: LineColor,
    },
    #[serde(rename = "kdj")]
    Kdj {
        #[serde(rename = "period")]
        period: u32,
        #[serde(rename = "smoothing_1")]
        smoothing_1: u32,
        #[serde(rename = "smoothing_2")]
        smoothing_2: u32,
        #[serde(rename = "k")]
        k: bool,
        #[serde(rename = "d")]
        d: bool,
        #[serde(rename = "j")]
        j: bool,
        #[serde(rename = "k_color")]
        k_color: LineColor,
        #[serde(rename = "d_color")]
        d_color: LineColor,
        #[serde(rename = "j_color")]
        j_color: LineColor,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndicatorEntry {
    pub id: IndicatorId,
    pub enabled: bool,
    pub settings: IndicatorSettings,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CandlestickIndicatorConfig {
    pub entries: Vec<IndicatorEntry>,
}

impl CandlestickIndicatorConfig {
    pub(crate) fn normalize(self) -> Self {
        let entries = INDICATOR_CATALOG
            .iter()
            .map(|definition| {
                let mut entry = self
                    .entries
                    .iter()
                    .find(|entry| entry.id == definition.id)
                    .cloned()
                    .unwrap_or_else(|| IndicatorEntry {
                        id: definition.id,
                        enabled: definition.enabled_by_default,
                        settings: (definition.default_settings)(),
                    });
                entry.settings = entry.settings.normalized(entry.id);
                entry
            })
            .collect();
        Self { entries }
    }

    pub(crate) fn warmup_depth(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.settings.warmup_depth(entry.id))
            .max()
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn entry(&self, id: IndicatorId) -> Option<&IndicatorEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub(crate) fn set_line_period(&mut self, id: IndicatorId, index: usize, period: u32) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        let Some(line) = entry
            .settings
            .lines_mut()
            .and_then(|lines| lines.rows.get_mut(index))
        else {
            return false;
        };
        line.period = period.clamp(PERIOD_MIN, PERIOD_MAX);
        true
    }

    pub(crate) fn set_line_color(
        &mut self,
        id: IndicatorId,
        index: usize,
        color: LineColor,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        let Some(line) = entry
            .settings
            .lines_mut()
            .and_then(|lines| lines.rows.get_mut(index))
        else {
            return false;
        };
        line.color = color;
        true
    }

    pub(crate) fn set_setting_period(
        &mut self,
        id: IndicatorId,
        key: SettingKey,
        period: u32,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry
            .settings
            .set_period(key, period.clamp(PERIOD_MIN, PERIOD_MAX))
    }

    pub(crate) fn set_switch(&mut self, id: IndicatorId, key: SettingKey, on: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.settings.set_switch(key, on)
    }

    pub(crate) fn set_color(&mut self, id: IndicatorId, key: SettingKey, color: LineColor) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.settings.set_color(key, color)
    }

    pub(crate) fn add_line(&mut self, id: IndicatorId, palette: &[Color]) -> bool {
        debug_assert!(!palette.is_empty());
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        let Some(lines) = entry.settings.lines_mut() else {
            return false;
        };
        lines.rows.push(IndicatorLineConfig {
            period: default_line_period(id),
            color: LineColor::from(palette[lines.rows.len() % palette.len()]),
        });
        true
    }

    pub(crate) fn remove_line(&mut self, id: IndicatorId, index: usize) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        let Some(lines) = entry.settings.lines_mut() else {
            return false;
        };
        if lines.rows.len() <= 1 || index >= lines.rows.len() {
            return false;
        }
        lines.rows.remove(index);
        true
    }
}

impl Default for CandlestickIndicatorConfig {
    fn default() -> Self {
        Self {
            entries: INDICATOR_CATALOG
                .iter()
                .map(|definition| IndicatorEntry {
                    id: definition.id,
                    enabled: definition.enabled_by_default,
                    settings: (definition.default_settings)(),
                })
                .collect(),
        }
    }
}

pub(crate) fn enabled_sub_plot_ids(config: &CandlestickIndicatorConfig) -> Vec<IndicatorId> {
    config
        .entries
        .iter()
        .filter(|entry| entry.enabled && definition_of(entry.id).pane == IndicatorPane::Sub)
        .map(|entry| entry.id)
        .collect()
}

impl IndicatorSettings {
    pub(crate) fn shape(&self) -> SettingsShape {
        match self {
            Self::None => SettingsShape::None,
            Self::Lines(_) => SettingsShape::Lines,
            Self::Macd { .. } => SettingsShape::Macd,
            Self::Kdj { .. } => SettingsShape::Kdj,
        }
    }

    pub(crate) fn lines(&self) -> Option<&LineListConfig> {
        match self {
            Self::Lines(lines) => Some(lines),
            _ => None,
        }
    }

    fn lines_mut(&mut self) -> Option<&mut LineListConfig> {
        match self {
            Self::Lines(lines) => Some(lines),
            _ => None,
        }
    }

    pub(crate) fn period(&self, key: SettingKey) -> Option<u32> {
        match (self, key) {
            (Self::Macd { short_period, .. }, SettingKey::ShortPeriod) => Some(*short_period),
            (Self::Macd { long_period, .. }, SettingKey::LongPeriod) => Some(*long_period),
            (Self::Macd { signal_period, .. }, SettingKey::SignalPeriod) => Some(*signal_period),
            (Self::Kdj { period, .. }, SettingKey::Period) => Some(*period),
            (Self::Kdj { smoothing_1, .. }, SettingKey::Smoothing1) => Some(*smoothing_1),
            (Self::Kdj { smoothing_2, .. }, SettingKey::Smoothing2) => Some(*smoothing_2),
            _ => None,
        }
    }

    fn set_period(&mut self, key: SettingKey, period: u32) -> bool {
        match (self, key) {
            (Self::Macd { short_period, .. }, SettingKey::ShortPeriod) => *short_period = period,
            (Self::Macd { long_period, .. }, SettingKey::LongPeriod) => *long_period = period,
            (Self::Macd { signal_period, .. }, SettingKey::SignalPeriod) => *signal_period = period,
            (Self::Kdj { period: value, .. }, SettingKey::Period) => *value = period,
            (Self::Kdj { smoothing_1, .. }, SettingKey::Smoothing1) => *smoothing_1 = period,
            (Self::Kdj { smoothing_2, .. }, SettingKey::Smoothing2) => *smoothing_2 = period,
            _ => return false,
        }
        true
    }

    pub(crate) fn switch(&self, key: SettingKey) -> Option<bool> {
        match (self, key) {
            (Self::Macd { dif, .. }, SettingKey::Dif) => Some(*dif),
            (Self::Macd { dea, .. }, SettingKey::Dea) => Some(*dea),
            (Self::Kdj { k, .. }, SettingKey::K) => Some(*k),
            (Self::Kdj { d, .. }, SettingKey::D) => Some(*d),
            (Self::Kdj { j, .. }, SettingKey::J) => Some(*j),
            _ => None,
        }
    }

    fn set_switch(&mut self, key: SettingKey, on: bool) -> bool {
        match (self, key) {
            (Self::Macd { dif, .. }, SettingKey::Dif) => *dif = on,
            (Self::Macd { dea, .. }, SettingKey::Dea) => *dea = on,
            (Self::Kdj { k, .. }, SettingKey::K) => *k = on,
            (Self::Kdj { d, .. }, SettingKey::D) => *d = on,
            (Self::Kdj { j, .. }, SettingKey::J) => *j = on,
            _ => return false,
        }
        true
    }

    pub(crate) fn color(&self, key: SettingKey) -> Option<LineColor> {
        match (self, key) {
            (Self::Macd { dif_color, .. }, SettingKey::DifColor) => Some(*dif_color),
            (Self::Macd { dea_color, .. }, SettingKey::DeaColor) => Some(*dea_color),
            (Self::Kdj { k_color, .. }, SettingKey::KColor) => Some(*k_color),
            (Self::Kdj { d_color, .. }, SettingKey::DColor) => Some(*d_color),
            (Self::Kdj { j_color, .. }, SettingKey::JColor) => Some(*j_color),
            _ => None,
        }
    }

    fn set_color(&mut self, key: SettingKey, color: LineColor) -> bool {
        match (self, key) {
            (Self::Macd { dif_color, .. }, SettingKey::DifColor) => *dif_color = color,
            (Self::Macd { dea_color, .. }, SettingKey::DeaColor) => *dea_color = color,
            (Self::Kdj { k_color, .. }, SettingKey::KColor) => *k_color = color,
            (Self::Kdj { d_color, .. }, SettingKey::DColor) => *d_color = color,
            (Self::Kdj { j_color, .. }, SettingKey::JColor) => *j_color = color,
            _ => return false,
        }
        true
    }

    fn normalized(mut self, id: IndicatorId) -> Self {
        if self.shape() != expected_shape(id) {
            return (definition_of(id).default_settings)();
        }
        if let Self::Lines(lines) = &mut self
            && lines.rows.is_empty()
        {
            lines.rows.push(IndicatorLineConfig {
                period: default_line_period(id),
                color: default_line_color(0),
            });
        }
        self.clamp_periods();
        self.normalize_colors();
        self
    }

    fn clamp_periods(&mut self) {
        match self {
            Self::None => {}
            Self::Lines(lines) => {
                for row in &mut lines.rows {
                    row.period = row.period.clamp(PERIOD_MIN, PERIOD_MAX);
                }
            }
            Self::Macd {
                short_period,
                long_period,
                signal_period,
                ..
            } => {
                *short_period = (*short_period).clamp(PERIOD_MIN, PERIOD_MAX);
                *long_period = (*long_period).clamp(PERIOD_MIN, PERIOD_MAX);
                *signal_period = (*signal_period).clamp(PERIOD_MIN, PERIOD_MAX);
            }
            Self::Kdj {
                period,
                smoothing_1,
                smoothing_2,
                ..
            } => {
                *period = (*period).clamp(PERIOD_MIN, PERIOD_MAX);
                *smoothing_1 = (*smoothing_1).clamp(PERIOD_MIN, PERIOD_MAX);
                *smoothing_2 = (*smoothing_2).clamp(PERIOD_MIN, PERIOD_MAX);
            }
        }
    }

    fn normalize_colors(&mut self) {
        match self {
            Self::None => {}
            Self::Lines(lines) => {
                for (index, row) in lines.rows.iter_mut().enumerate() {
                    row.color = row.color.normalized(default_line_color(index));
                }
            }
            Self::Macd {
                dif_color,
                dea_color,
                ..
            } => {
                *dif_color = dif_color.normalized(default_line_color(0));
                *dea_color = dea_color.normalized(default_line_color(1));
            }
            Self::Kdj {
                k_color,
                d_color,
                j_color,
                ..
            } => {
                *k_color = k_color.normalized(default_line_color(0));
                *d_color = d_color.normalized(default_line_color(1));
                *j_color = j_color.normalized(default_line_color(2));
            }
        }
    }

    pub(crate) fn warmup_depth(&self, id: IndicatorId) -> usize {
        settings_depth(id, self)
    }
}

pub(crate) fn expected_shape(id: IndicatorId) -> SettingsShape {
    match id {
        IndicatorId::Ma | IndicatorId::Ema | IndicatorId::Rsi => SettingsShape::Lines,
        IndicatorId::Vol => SettingsShape::None,
        IndicatorId::Macd => SettingsShape::Macd,
        IndicatorId::Kdj => SettingsShape::Kdj,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsShape {
    None,
    Lines,
    Macd,
    Kdj,
}

pub(crate) fn line_depth(id: IndicatorId, period: u32) -> usize {
    match id {
        IndicatorId::Rsi => period as usize,
        _ => (period as usize).saturating_sub(1),
    }
}

pub(crate) fn settings_depth(id: IndicatorId, settings: &IndicatorSettings) -> usize {
    match settings {
        IndicatorSettings::None => 0,
        IndicatorSettings::Lines(lines) => lines
            .rows
            .iter()
            .map(|row| line_depth(id, row.period))
            .max()
            .unwrap_or(0),
        IndicatorSettings::Macd {
            short_period,
            long_period,
            ..
        } => (*short_period.max(long_period) as usize).saturating_sub(1),
        IndicatorSettings::Kdj {
            period,
            smoothing_1,
            smoothing_2,
            ..
        } => {
            (*period as usize).saturating_sub(1)
                + (*smoothing_1 as usize).saturating_sub(1)
                + (*smoothing_2 as usize).saturating_sub(1)
        }
    }
}

pub(crate) fn normalize_indicator<'de, D>(
    deserializer: D,
) -> Result<CandlestickIndicatorConfig, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(CandlestickIndicatorConfig::deserialize(deserializer)?.normalize())
}

fn default_line_period(id: IndicatorId) -> u32 {
    let IndicatorSettings::Lines(lines) = (definition_of(id).default_settings)() else {
        panic!("only a line-list indicator can hold a `Lines` setting");
    };
    lines
        .rows
        .first()
        .expect("every catalogue line list has a default line")
        .period
}

use serde::{Deserialize, Serialize};
use ui_core::paint::color::Color;

use super::config::{
    IndicatorLineConfig, IndicatorSettings, LineColor, LineListConfig, PERIOD_MAX, PERIOD_MIN,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum IndicatorId {
    #[serde(rename = "ma")]
    Ma,
    #[serde(rename = "ema")]
    Ema,
    #[serde(rename = "vol")]
    Vol,
    #[serde(rename = "macd")]
    Macd,
    #[serde(rename = "rsi")]
    Rsi,
    #[serde(rename = "kdj")]
    Kdj,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndicatorPane {
    Main,
    Sub,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingKey {
    Period,
    ShortPeriod,
    LongPeriod,
    SignalPeriod,
    Smoothing1,
    Smoothing2,
    Lines,
    Dif,
    Dea,
    K,
    D,
    J,
    DifColor,
    DeaColor,
    KColor,
    DColor,
    JColor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingControl {
    Period { min: u32, max: u32 },
    Switch,
    LineList { min_rows: usize },
}

pub(crate) struct SettingRow {
    pub key: SettingKey,
    pub label_key: &'static str,
    pub control: SettingControl,
    pub color_key: Option<SettingKey>,
}

pub(crate) struct IndicatorDefinition {
    pub id: IndicatorId,
    pub name_key: &'static str,
    pub pane: IndicatorPane,
    pub rows: &'static [SettingRow],
    pub enabled_by_default: bool,
    pub default_settings: fn() -> IndicatorSettings,
}

pub(crate) fn definition_of(id: IndicatorId) -> &'static IndicatorDefinition {
    INDICATOR_CATALOG
        .iter()
        .find(|definition| definition.id == id)
        .expect("the catalogue declares every indicator id")
}

const MA_DEFAULT_PERIODS: [u32; 3] = [5, 10, 20];
const EMA_DEFAULT_PERIODS: [u32; 1] = [12];
const RSI_DEFAULT_PERIODS: [u32; 3] = [6, 12, 24];

fn lines_from(periods: &[u32]) -> IndicatorSettings {
    IndicatorSettings::Lines(LineListConfig {
        rows: periods
            .iter()
            .enumerate()
            .map(|(index, period)| IndicatorLineConfig {
                period: *period,
                color: default_line_color(index),
            })
            .collect(),
    })
}

const DEFAULT_LINE_COLORS: [[u8; 4]; 3] = [
    [0xE8, 0xA8, 0x3C, 0xFF],
    [0x8A, 0x6C, 0xE8, 0xFF],
    [0x3C, 0xB8, 0xC8, 0xFF],
];

pub(crate) fn default_line_color(index: usize) -> LineColor {
    let color = Color::from_rgba8(DEFAULT_LINE_COLORS[index % DEFAULT_LINE_COLORS.len()]);
    LineColor::from(color)
}

fn ma_default_settings() -> IndicatorSettings {
    lines_from(&MA_DEFAULT_PERIODS)
}

fn ema_default_settings() -> IndicatorSettings {
    lines_from(&EMA_DEFAULT_PERIODS)
}

fn vol_default_settings() -> IndicatorSettings {
    IndicatorSettings::None
}

fn macd_default_settings() -> IndicatorSettings {
    IndicatorSettings::Macd {
        short_period: 12,
        long_period: 26,
        signal_period: 9,
        dif: true,
        dea: true,
        dif_color: default_line_color(0),
        dea_color: default_line_color(1),
    }
}

fn rsi_default_settings() -> IndicatorSettings {
    lines_from(&RSI_DEFAULT_PERIODS)
}

fn kdj_default_settings() -> IndicatorSettings {
    IndicatorSettings::Kdj {
        period: 9,
        smoothing_1: 3,
        smoothing_2: 3,
        k: true,
        d: true,
        j: true,
        k_color: default_line_color(0),
        d_color: default_line_color(1),
        j_color: default_line_color(2),
    }
}

pub(crate) const INDICATOR_CATALOG: &[IndicatorDefinition] = &[
    IndicatorDefinition {
        id: IndicatorId::Ma,
        name_key: "widget.candlestick.indicator.ma",
        pane: IndicatorPane::Main,
        enabled_by_default: true,
        default_settings: ma_default_settings,
        rows: &[SettingRow {
            key: SettingKey::Lines,
            label_key: "widget.candlestick.indicator.lines",
            control: SettingControl::LineList { min_rows: 1 },
            color_key: None,
        }],
    },
    IndicatorDefinition {
        id: IndicatorId::Ema,
        name_key: "widget.candlestick.indicator.ema",
        pane: IndicatorPane::Main,
        enabled_by_default: false,
        default_settings: ema_default_settings,
        rows: &[SettingRow {
            key: SettingKey::Lines,
            label_key: "widget.candlestick.indicator.lines",
            control: SettingControl::LineList { min_rows: 1 },
            color_key: None,
        }],
    },
    IndicatorDefinition {
        id: IndicatorId::Vol,
        name_key: "widget.candlestick.indicator.vol",
        pane: IndicatorPane::Sub,
        enabled_by_default: true,
        default_settings: vol_default_settings,
        rows: &[],
    },
    IndicatorDefinition {
        id: IndicatorId::Macd,
        name_key: "widget.candlestick.indicator.macd",
        pane: IndicatorPane::Sub,
        enabled_by_default: false,
        default_settings: macd_default_settings,
        rows: &[
            SettingRow {
                key: SettingKey::ShortPeriod,
                label_key: "widget.candlestick.indicator.short_period",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
                color_key: None,
            },
            SettingRow {
                key: SettingKey::LongPeriod,
                label_key: "widget.candlestick.indicator.long_period",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
                color_key: None,
            },
            SettingRow {
                key: SettingKey::SignalPeriod,
                label_key: "widget.candlestick.indicator.signal_period",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
                color_key: None,
            },
            SettingRow {
                key: SettingKey::Dif,
                label_key: "widget.candlestick.indicator.dif",
                control: SettingControl::Switch,
                color_key: Some(SettingKey::DifColor),
            },
            SettingRow {
                key: SettingKey::Dea,
                label_key: "widget.candlestick.indicator.dea",
                control: SettingControl::Switch,
                color_key: Some(SettingKey::DeaColor),
            },
        ],
    },
    IndicatorDefinition {
        id: IndicatorId::Rsi,
        name_key: "widget.candlestick.indicator.rsi",
        pane: IndicatorPane::Sub,
        enabled_by_default: false,
        default_settings: rsi_default_settings,
        rows: &[SettingRow {
            key: SettingKey::Lines,
            label_key: "widget.candlestick.indicator.lines",
            control: SettingControl::LineList { min_rows: 1 },
            color_key: None,
        }],
    },
    IndicatorDefinition {
        id: IndicatorId::Kdj,
        name_key: "widget.candlestick.indicator.kdj",
        pane: IndicatorPane::Sub,
        enabled_by_default: false,
        default_settings: kdj_default_settings,
        rows: &[
            SettingRow {
                key: SettingKey::Period,
                label_key: "widget.candlestick.indicator.period",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
                color_key: None,
            },
            SettingRow {
                key: SettingKey::Smoothing1,
                label_key: "widget.candlestick.indicator.smoothing_1",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
                color_key: None,
            },
            SettingRow {
                key: SettingKey::Smoothing2,
                label_key: "widget.candlestick.indicator.smoothing_2",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
                color_key: None,
            },
            SettingRow {
                key: SettingKey::K,
                label_key: "widget.candlestick.indicator.k",
                control: SettingControl::Switch,
                color_key: Some(SettingKey::KColor),
            },
            SettingRow {
                key: SettingKey::D,
                label_key: "widget.candlestick.indicator.d",
                control: SettingControl::Switch,
                color_key: Some(SettingKey::DColor),
            },
            SettingRow {
                key: SettingKey::J,
                label_key: "widget.candlestick.indicator.j",
                control: SettingControl::Switch,
                color_key: Some(SettingKey::JColor),
            },
        ],
    },
];

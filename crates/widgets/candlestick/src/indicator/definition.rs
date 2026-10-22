//! The static catalogue: which indicators exist, in which order, and what each
//! one defaults to.
//!
//! The catalogue is the single source for two things: the order of the panel and
//! of the stacked sub plots, and the default settings a configuration is
//! completed with. Everything here is `const` data — no `OnceLock`, no runtime
//! lookup table — so a lookup is an array scan over six entries.

use gpui_kit::{Hsla, rgba};
use serde::{Deserialize, Serialize};

use super::config::{
    IndicatorLineConfig, IndicatorSettings, LineColor, LineListConfig, PERIOD_MAX, PERIOD_MIN,
};

/// An indicator's identity in the catalogue.
///
/// It is the persisted identity: once written into a configuration it may not be
/// renamed. The display name is an i18n key, kept separate from it.
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

/// Which region of the chart an indicator draws in.
///
/// The catalogue owns this: both the main-chart drawing loop and the sub-plot
/// loop filter the enabled series by it, so an indicator cannot be drawn in one
/// region by one call site and another region by the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndicatorPane {
    Main,
    Sub,
}

/// The identity of one settings row inside its indicator.
///
/// An enum rather than a `SmolStr`: which settings an indicator has is a
/// compile-time fact. It is not persisted — the persisted names are the field
/// names of [`IndicatorSettings`], which serde owns — so it derives no serde
/// implementation.
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
}

/// The shape of the control a settings row is drawn as.
///
/// Three variants and no extension point: every settings item the catalogue
/// declares is one of these. An indicator that needs another shape adds a
/// variant and makes the panel handle it, rather than growing an abstraction
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingControl {
    /// An integer input, for the period-like parameters.
    Period { min: u32, max: u32 },
    /// A checkbox, for "draw this line or not".
    Switch,
    /// A list of lines: each row is a period, a colour and a delete button, with
    /// one "+" button under the last row.
    LineList { min_rows: usize },
}

/// One settings row as the panel draws it.
pub(crate) struct SettingRow {
    /// This row's identity inside its indicator: the key of the input entity it
    /// owns, the selector a test reads, and half of what pairs the row with the
    /// settings value it edits. It is not persisted.
    pub key: SettingKey,
    /// The i18n key of the row's label.
    pub label_key: &'static str,
    /// The control this row is drawn as.
    pub control: SettingControl,
}

/// A full declaration of one indicator.
///
/// `default_settings` is a function pointer rather than a closure: a closure
/// that captured anything could not live in a `const`, while a function pointer
/// can — and it still lets the default value be built at runtime, which the
/// `Vec` inside a line list needs.
pub(crate) struct IndicatorDefinition {
    pub id: IndicatorId,
    /// The i18n key of the title the panel lists this indicator under. The chart
    /// legend spells its own labels; this one is the panel's.
    pub name_key: &'static str,
    /// Which region of the chart the indicator draws in.
    pub pane: IndicatorPane,
    /// The settings rows the panel draws under this indicator, top to bottom.
    /// Empty means the indicator has nothing to configure.
    pub rows: &'static [SettingRow],
    /// Whether a configuration starts with this indicator enabled.
    pub enabled_by_default: bool,
    /// The default settings value. Its shape must be the one `expected_shape`
    /// declares for this indicator; `definition_tests` pins the two together.
    pub default_settings: fn() -> IndicatorSettings,
}

/// The declaration of `id` in the catalogue.
///
/// The catalogue's order is the panel's order and the sub plots' stacking order,
/// so the order of the returned slice matters.
pub(crate) fn definition_of(id: IndicatorId) -> &'static IndicatorDefinition {
    INDICATOR_CATALOG
        .iter()
        .find(|definition| definition.id == id)
        .expect("the catalogue declares every indicator id")
}

// The defaults are fixed `const` arrays so that the whole table stays `const`:
// neither `vec![]` nor `Vec::push` is callable from a `const fn`. Turning them
// into a `Vec` happens at runtime, inside the `fn` the catalogue stores.
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
                // A fixed snapshot, not a theme lookup and not a random colour:
                // `Default::default()` must be idempotent, otherwise the
                // configuration's envelope changes on every start. A line the
                // user adds later takes the current palette instead — see
                // `CandlestickIndicatorConfig::add_line`.
                color: default_line_color(index),
            })
            .collect(),
    })
}

/// The fixed colours of the default lines, as big-endian RGBA.
///
/// These are the colours a configuration starts with — the shipped chart's amber,
/// violet and cyan. A line the user adds takes a snapshot of the current
/// appearance's chart-series palette instead (see
/// `CandlestickIndicatorConfig::add_line`), and the stored colour never follows
/// the theme afterwards.
///
/// They are compile-time constants rather than theme roles because `Default` has
/// no `App` to read a theme from, and `CandlestickConfig::default()` is called
/// exactly where there is no `cx` (`VersionedWidgetConfig::decode`, tests).
const DEFAULT_LINE_COLOR_RGBA: [u32; 3] = [0xE8A83CFF, 0x8A6CE8FF, 0x3CB8C8FF];

/// The default colour of the line at `index`, cycling through the three fixed
/// colours.
pub(crate) fn default_line_color(index: usize) -> LineColor {
    let color = rgba(DEFAULT_LINE_COLOR_RGBA[index % DEFAULT_LINE_COLOR_RGBA.len()]);
    LineColor::from(Hsla::from(color))
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

/// Every indicator the chart knows, in panel and sub-plot order.
///
/// MA → EMA → VOL → MACD → RSI → KDJ. The order is load-bearing: the config's
/// entries are normalized to it, the panel lists them in it and the sub plots
/// stack in it.
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
        }],
    },
    IndicatorDefinition {
        id: IndicatorId::Vol,
        name_key: "widget.candlestick.indicator.vol",
        pane: IndicatorPane::Sub,
        enabled_by_default: true,
        default_settings: vol_default_settings,
        // Volume has nothing to configure, which is what makes it the one
        // indicator that is neither expandable nor collapsible.
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
            },
            SettingRow {
                key: SettingKey::LongPeriod,
                label_key: "widget.candlestick.indicator.long_period",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
            },
            SettingRow {
                key: SettingKey::SignalPeriod,
                label_key: "widget.candlestick.indicator.signal_period",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
            },
            SettingRow {
                key: SettingKey::Dif,
                label_key: "widget.candlestick.indicator.dif",
                control: SettingControl::Switch,
            },
            SettingRow {
                key: SettingKey::Dea,
                label_key: "widget.candlestick.indicator.dea",
                control: SettingControl::Switch,
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
            },
            SettingRow {
                key: SettingKey::Smoothing1,
                label_key: "widget.candlestick.indicator.smoothing_1",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
            },
            SettingRow {
                key: SettingKey::Smoothing2,
                label_key: "widget.candlestick.indicator.smoothing_2",
                control: SettingControl::Period {
                    min: PERIOD_MIN,
                    max: PERIOD_MAX,
                },
            },
            SettingRow {
                key: SettingKey::K,
                label_key: "widget.candlestick.indicator.k",
                control: SettingControl::Switch,
            },
            SettingRow {
                key: SettingKey::D,
                label_key: "widget.candlestick.indicator.d",
                control: SettingControl::Switch,
            },
            SettingRow {
                key: SettingKey::J,
                label_key: "widget.candlestick.indicator.j",
                control: SettingControl::Switch,
            },
        ],
    },
];

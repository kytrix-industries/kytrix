//! The static catalogue: which indicators exist, in which order, and what each
//! one defaults to.
//!
//! The catalogue is the single source for two things: the order of the panel and
//! of the stacked sub plots, and the default settings a configuration is
//! completed with. Everything here is `const` data — no `OnceLock`, no runtime
//! lookup table — so a lookup is an array scan over six entries.

use serde::{Deserialize, Serialize};

use super::config::{IndicatorLineConfig, IndicatorSettings, LineListConfig};

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

/// A full declaration of one indicator.
///
/// `default_settings` is a function pointer rather than a closure: a closure
/// that captured anything could not live in a `const`, while a function pointer
/// can — and it still lets the default value be built at runtime, which the
/// `Vec` inside a line list needs.
pub(crate) struct IndicatorDefinition {
    pub id: IndicatorId,
    /// Which region of the chart the indicator draws in.
    pub pane: IndicatorPane,
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
                // The default colour is a fixed constant, not a random one and
                // not one read from the theme: `Default::default()` must be
                // idempotent, otherwise the configuration's envelope changes on
                // every start. Randomness only happens when the user adds a line
                // — see the design's §10.2.
                color: default_line_color(index),
            })
            .collect(),
    })
}

/// The fixed colours of the default lines.
///
/// Deliberately compile-time constants rather than theme roles: `Default` has no
/// `App` to read a theme from, and `CandlestickConfig::default()` is called
/// exactly where there is no `cx` (`VersionedWidgetConfig::decode`, tests). This
/// is the recorded deviation of the design's §10.2, not an oversight.
pub(crate) const DEFAULT_LINE_COLORS: [[u8; 4]; 3] = [
    [0xE8, 0xA8, 0x3C, 0xFF],
    [0x8A, 0x6C, 0xE8, 0xFF],
    [0x3C, 0xB8, 0xC8, 0xFF],
];

pub(crate) fn default_line_color(index: usize) -> [u8; 4] {
    DEFAULT_LINE_COLORS[index % DEFAULT_LINE_COLORS.len()]
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
        pane: IndicatorPane::Main,
        enabled_by_default: true,
        default_settings: ma_default_settings,
    },
    IndicatorDefinition {
        id: IndicatorId::Ema,
        pane: IndicatorPane::Main,
        enabled_by_default: false,
        default_settings: ema_default_settings,
    },
    IndicatorDefinition {
        id: IndicatorId::Vol,
        pane: IndicatorPane::Sub,
        enabled_by_default: true,
        default_settings: vol_default_settings,
    },
    IndicatorDefinition {
        id: IndicatorId::Macd,
        pane: IndicatorPane::Sub,
        enabled_by_default: false,
        default_settings: macd_default_settings,
    },
    IndicatorDefinition {
        id: IndicatorId::Rsi,
        pane: IndicatorPane::Sub,
        enabled_by_default: false,
        default_settings: rsi_default_settings,
    },
    IndicatorDefinition {
        id: IndicatorId::Kdj,
        pane: IndicatorPane::Sub,
        enabled_by_default: false,
        default_settings: kdj_default_settings,
    },
];

// TODO(§6): the panel's settings rows (`SettingRow`, `SettingControl`,
// `SettingKey` and `IndicatorDefinition::rows`) land with their first reader,
// the panel's editor. Declaring them before anything reads them would be dead
// code, so this stage's catalogue carries only what the configuration, the
// computation and the main-chart painter use.

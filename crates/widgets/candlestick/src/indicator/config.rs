//! The persisted indicator configuration and its normalization.
//!
//! The types here are the disk format: one entry per indicator, in catalogue
//! order, plus the per-indicator settings value. A configuration that came from
//! disk is untrusted — it may be older, hand-edited or written by a build with a
//! different catalogue — so [`CandlestickIndicatorConfig::normalize`] restores
//! the invariants the type documents.

use gpui_kit::{Rgba, hsla};
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    INDICATOR_CATALOG, IndicatorId, IndicatorPane, SettingKey, default_line_color, definition_of,
};

/// The smallest period a setting may take.
///
/// The range is one answer rather than three: [`CandlestickIndicatorConfig::normalize`]
/// clamps stored values into it, the catalogue's `Period` rows are declared with it, and
/// the panel rejects typed text outside it. The `period_invalid` message names the same
/// two numbers, which a test pins.
pub(crate) const PERIOD_MIN: u32 = 1;

/// The largest period a setting may take.
pub(crate) const PERIOD_MAX: u32 = 500;

/// One indicator line's settings.
///
/// `color` stores sRGB rather than `Hsla`: `Hsla` is not a serde type, and sRGB
/// reads back under either appearance; the painter converts it when drawing.
///
/// Where the colour comes from depends on where the line came from (design
/// §10.2): a default line uses one of the fixed `DEFAULT_LINE_COLORS`, a line
/// the user added gets one random colour at creation time and keeps it from then
/// on. This is the recorded temporary deviation, not an oversight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IndicatorLineConfig {
    pub period: u32,
    pub color: [u8; 4],
}

/// A dynamic list of lines.
///
/// `rows` is never empty: the delete button is disabled on the last row, and
/// both construction and decoding guarantee it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LineListConfig {
    pub rows: Vec<IndicatorLineConfig>,
}

/// One indicator's settings value.
///
/// The shape of the variant must match the indicator's catalogue rows; the tests
/// pin that through `expected_shape`.
///
/// Every field is renamed explicitly: the workspace forbids
/// `#[serde(rename_all)]`, so the names are written out rather than inferred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    },
}

/// One indicator's entry in a configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IndicatorEntry {
    pub id: IndicatorId,
    pub enabled: bool,
    pub settings: IndicatorSettings,
}

/// A complete, normalized indicator configuration.
///
/// Invariants, held after construction and after decoding (pinned by the tests):
/// 1. `entries` covers the catalogue exactly — same order, no duplicate, no
///    missing entry;
/// 2. every `settings` has the shape the indicator's catalogue rows edit;
/// 3. every `Lines.rows` is non-empty;
/// 4. every period is within [`PERIOD_MIN`]`..=`[`PERIOD_MAX`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CandlestickIndicatorConfig {
    /// One entry per indicator, in catalogue order. The panel's open state is
    /// not part of it: the design keeps that out of persistence.
    pub entries: Vec<IndicatorEntry>,
}

impl CandlestickIndicatorConfig {
    /// Restores every invariant this type documents.
    ///
    /// This is the deserialization seam of `CandlestickConfig::indicator` (see
    /// [`normalize_indicator`]); decoding alone would hand back whatever the
    /// payload said.
    ///
    /// The six rules run in the design's order. Rules 1–3 are one scan of the
    /// catalogue, rules 4–6 are per entry.
    pub(crate) fn normalize(self) -> Self {
        // Rules 1–3: the catalogue is both the order and the set. Looking every
        // id up in the configured entries reorders them and fills in the missing
        // ones; `find` keeps the first of a duplicate.
        //
        // Ids outside the catalogue are deliberately not handled: `IndicatorId`
        // is an enum, so an unknown name fails to deserialize and can never
        // reach this function. A `retain` for "unknown ids" would be code that
        // never runs, and would make readers believe unknown ids survive
        // decoding.
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

    /// The longest warmup requirement among the enabled indicators, in candles.
    ///
    /// The history prefetch uses it as its threshold. The numbers come from
    /// [`settings_depth`], the same function that answers every one of an
    /// indicator's `IndicatorLine::readable_from` values, so the fetch threshold
    /// and the display thresholds cannot drift apart.
    pub(crate) fn warmup_depth(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.settings.warmup_depth(entry.id))
            .max()
            .unwrap_or(0)
    }

    /// The entry of `id`, if the configuration holds one.
    pub(crate) fn entry(&self, id: IndicatorId) -> Option<&IndicatorEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Sets the period of one line of `id`'s list.
    ///
    /// Returns whether the value was applied: the index is `Some` only for a
    /// line list the panel drew, and a stale one is a value that is not there.
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

    /// Sets the value of one named period setting of `id`, such as MACD's short
    /// period.
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

    /// Sets one named switch of `id`, such as MACD's DIF.
    pub(crate) fn set_switch(&mut self, id: IndicatorId, key: SettingKey, on: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.settings.set_switch(key, on)
    }

    /// Appends a line to `id`'s list.
    ///
    /// The period is the indicator's own default, so a new line starts somewhere
    /// the user recognizes rather than at an arbitrary value; the colour is the
    /// random one the design gives a line the user asked for (design §10.2).
    /// Returns whether the line was added: only a line-list setting has room for
    /// one.
    pub(crate) fn add_line(&mut self, id: IndicatorId) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        let Some(lines) = entry.settings.lines_mut() else {
            return false;
        };
        lines.rows.push(IndicatorLineConfig {
            period: default_line_period(id),
            color: random_line_color(),
        });
        true
    }

    /// Removes one line from `id`'s list.
    ///
    /// The last line is never removed: a line list is never empty, and the panel
    /// disables the button that would ask for it. Returns whether a line was
    /// removed.
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
        // Built from the catalogue, so it satisfies the invariants by
        // construction and needs no normalization.
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

/// The enabled sub-plot indicators, in catalogue order.
///
/// The entries are normalized to catalogue order, so filtering them preserves the
/// stacking order. This is the one answer to "how many sub plots are there, and
/// which indicator owns each". The layout and the painting loop both zip their
/// boxes against it, so counting sub plots any other way (for example by
/// filtering the computed series, which never contains VOL) would put the wrong
/// indicator in a box.
pub(crate) fn enabled_sub_plot_ids(config: &CandlestickIndicatorConfig) -> Vec<IndicatorId> {
    config
        .entries
        .iter()
        .filter(|entry| entry.enabled && definition_of(entry.id).pane == IndicatorPane::Sub)
        .map(|entry| entry.id)
        .collect()
}

impl IndicatorSettings {
    /// The settings shape this value holds.
    pub(crate) fn shape(&self) -> SettingsShape {
        match self {
            Self::None => SettingsShape::None,
            Self::Lines(_) => SettingsShape::Lines,
            Self::Macd { .. } => SettingsShape::Macd,
            Self::Kdj { .. } => SettingsShape::Kdj,
        }
    }

    /// The line list this value holds, if it holds one.
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

    /// The period one named setting holds.
    ///
    /// `None` means this value has no such setting: the panel only asks about
    /// keys the indicator's catalogue rows declare, so an answer of `None` is a
    /// row and a value that have drifted apart rather than a user error.
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

    /// Writes one named period setting. Returns whether this value has it.
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

    /// Whether one named switch is on.
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

    /// Writes one named switch. Returns whether this value has it.
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

    /// Rules 4–6 of [`CandlestickIndicatorConfig::normalize`], in order.
    fn normalized(mut self, id: IndicatorId) -> Self {
        // Rule 4: the value's shape must be the one the catalogue declares for
        // this indicator; anything else is replaced by the whole default value.
        if self.shape() != expected_shape(id) {
            return (definition_of(id).default_settings)();
        }
        // Rule 5: a line list never stays empty. The replacement row takes the
        // indicator's default period and the first fixed colour — repairing a
        // configuration never invents a random colour (design §10.2).
        if let Self::Lines(lines) = &mut self
            && lines.rows.is_empty()
        {
            lines.rows.push(IndicatorLineConfig {
                period: default_line_period(id),
                color: default_line_color(0),
            });
        }
        // Rule 6: out-of-range periods are clamped per item; the settings value
        // is not reset wholesale.
        self.clamp_periods();
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

    /// How many leading candles this indicator needs before its values are
    /// trustworthy.
    ///
    /// `Lines` is shared by MA/EMA/RSI and their depths differ, which is why the
    /// indicator identity is a parameter.
    pub(crate) fn warmup_depth(&self, id: IndicatorId) -> usize {
        settings_depth(id, self)
    }
}

/// The settings shape a catalogue entry must hold.
///
/// The catalogue's `rows` describe what the panel draws; this says which
/// [`IndicatorSettings`] variant those rows edit. They are two different enums,
/// so the correspondence is written once here rather than inferred from `rows`.
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

/// How many leading candles one line of `id` with `period` needs before its
/// values are trustworthy.
///
/// This is the single-line half of the design's §7.2 depth table: MA and EMA
/// need `n - 1` leading candles, RSI needs `n`.
///
/// The depth is a property of the **line's own period**, not of the indicator:
/// MA with lines 5/10/20 is readable from 4, 9 and 19 respectively. Taking the
/// indicator's longest depth for every line would hide MA5 for fifteen extra
/// candles and, worse, would make the display threshold and the value history
/// disagree in a way no test could see.
pub(crate) fn line_depth(id: IndicatorId, period: u32) -> usize {
    match id {
        // RSI's first value is the first change, so it needs one more candle
        // than the moving averages do.
        IndicatorId::Rsi => period as usize,
        _ => (period as usize).saturating_sub(1),
    }
}

/// How many leading candles the whole `settings` value needs before every one of
/// its values is trustworthy.
///
/// One function, two call sites: the history prefetch threshold
/// (`CandlestickIndicatorConfig::warmup_depth`) and the per-line
/// `IndicatorLine::readable_from` values, which are computed by applying
/// [`line_depth`] to each row. The numbers are the design's §7.2 table: MA
/// `n - 1`, EMA `n - 1`, RSI `n`, MACD `max(short, long) - 1`, KDJ
/// `(n - 1) + (m1 - 1) + (m2 - 1)` and VOL `0`.
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

/// The deserialization seam of `CandlestickConfig::indicator`.
///
/// Without the `deserialize_with` attribute that names this function, the
/// invariants of [`CandlestickIndicatorConfig`] would be an empty promise: what
/// came from disk would enter memory untouched.
pub(crate) fn normalize_indicator<'de, D>(
    deserializer: D,
) -> Result<CandlestickIndicatorConfig, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(CandlestickIndicatorConfig::deserialize(deserializer)?.normalize())
}

/// The period an empty line list is repaired with.
///
/// It is the first period of the indicator's own defaults. Rule 4 has already
/// established that this indicator holds a `Lines` value, and the catalogue's
/// `Lines` indicators all default to a non-empty line list — `definition_tests`
/// pins that half of the promise.
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

/// A colour for a line the user just added.
///
/// This is the temporary measure the design's §10.2 records: a line the user adds
/// gets a random colour instead of a theme role, and that colour is stored from
/// then on. Only the *new* line is unpredictable, which is why the defaults are
/// fixed constants instead.
fn random_line_color() -> [u8; 4] {
    let hue = rand::random_range(0.0_f32..1.0);
    // Saturation and lightness are clamped to a readable band; the magnitudes
    // follow the contrast the theme asks of `chart.series[n]` (`roles.rs:556-573`).
    let saturation = rand::random_range(0.45_f32..=0.75);
    let lightness = rand::random_range(0.45_f32..=0.70);
    let rgba = Rgba::from(hsla(hue, saturation, lightness, 1.0));
    [
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8,
        (rgba.a * 255.0).round() as u8,
    ]
}

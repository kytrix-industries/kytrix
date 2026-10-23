//! The persisted indicator configuration and its normalization.
//!
//! The types here are the disk format: one entry per indicator, in catalogue
//! order, plus the per-indicator settings value. A configuration that came from
//! disk is untrusted — it may be older, hand-edited or written by a build with a
//! different catalogue — so [`CandlestickIndicatorConfig::normalize`] restores
//! the invariants the type documents.

use std::fmt;

use gpui_kit::{Hsla, hsla};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};

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

/// One indicator line's colour, as the four HSLA components.
///
/// Stored as `f32` rather than eight-bit sRGB so a colour the picker chose round-trips exactly.
/// Every component is a fraction in `0..=1`; a value read from disk is normalized into that range.
///
/// The stored shape is a map with named components. A legacy `[u8; 4]` sRGB array
/// must not decode: serde's derived struct deserializer would accept a sequence
/// and read the bytes as four fractions, which is why [`Deserialize`] is
/// implemented by hand below.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct LineColor {
    pub h: f32,
    pub s: f32,
    pub l: f32,
    pub a: f32,
}

impl From<Hsla> for LineColor {
    fn from(color: Hsla) -> Self {
        Self {
            h: color.h,
            s: color.s,
            l: color.l,
            a: color.a,
        }
    }
}

impl From<LineColor> for Hsla {
    fn from(color: LineColor) -> Self {
        hsla(color.h, color.s, color.l, color.a)
    }
}

impl<'de> Deserialize<'de> for LineColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        /// The map shape, reached only through a [`MapAccess`].
        ///
        /// A derive on [`LineColor`] itself would also accept a sequence — serde
        /// structs do — and would read the components of a legacy `[u8; 4]` sRGB
        /// array as four fractions instead of rejecting the old format.
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
    /// This colour with every component restored to its documented range.
    ///
    /// A value from disk is untrusted. A non-finite component cannot be compared
    /// meaningfully (equality would poison the panel's "is this a change?" check)
    /// or painted, so it replaces the whole colour with `fallback`. A finite hue
    /// outside `0..=1` is the same colour wrapped around the wheel; saturation,
    /// lightness and alpha are clamped, because their scale has no wrap.
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

/// One indicator line's settings.
///
/// `color` is the line's own stored colour, never a theme lookup: a default line
/// starts from [`super::default_line_color`], a line the user adds takes a
/// snapshot of the appearance's chart-series palette at that moment, and the
/// stored colour does not follow the theme afterwards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndicatorLineConfig {
    pub period: u32,
    pub color: LineColor,
}

/// A dynamic list of lines.
///
/// `rows` is never empty: the last line cannot be removed, both construction and
/// decoding refuse an empty list, and at that minimum the panel draws no delete
/// button at all — so there is no control whose press could ask for the removal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

/// One indicator's entry in a configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
/// 4. every period is within [`PERIOD_MIN`]`..=`[`PERIOD_MAX`];
/// 5. every colour is finite and its components are inside `0..=1`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// The seven rules run in the design's order. Rules 1–3 are one scan of the
    /// catalogue, rules 4–7 are per entry.
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

    /// Sets the colour of one line of `id`'s list.
    ///
    /// Returns whether the value was applied: the index is `Some` only for a
    /// line list the panel drew, and a stale one is a line that is not there.
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

    /// Sets one named colour of `id`, such as MACD's DIF colour.
    ///
    /// Returns whether the value was applied: a key that is not one of the
    /// colour settings, or one this indicator's shape does not hold, is a row and
    /// a value that have drifted apart rather than a colour to write.
    pub(crate) fn set_color(&mut self, id: IndicatorId, key: SettingKey, color: LineColor) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.settings.set_color(key, color)
    }

    /// Appends a line to `id`'s list.
    ///
    /// The period is the indicator's own default, so a new line starts somewhere
    /// the user recognizes rather than at an arbitrary value. The new line's
    /// colour is `palette[lines.rows.len() % palette.len()]`: `palette` is the
    /// current appearance's chart-series palette, which the panel reads from the
    /// theme and passes in. The chosen colour is stored as a snapshot rather than
    /// looked up from the theme later.
    ///
    /// Returns whether the line was added: only a line-list setting has room for
    /// one.
    ///
    /// # Panics
    ///
    /// Panics if `palette` is empty: the panel always passes the current
    /// appearance's non-empty palette, and a line added without one would have no
    /// colour to store.
    pub(crate) fn add_line(&mut self, id: IndicatorId, palette: &[Hsla]) -> bool {
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

    /// Removes one line from `id`'s list.
    ///
    /// The last line is never removed: a line list is never empty, and at the
    /// minimum the panel draws no delete button to ask for it. Returns whether a
    /// line was removed.
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

    /// The colour one named setting holds.
    ///
    /// Only the switch rows' colours are named settings: a line list's colours
    /// belong to its rows, not to the list. `None` therefore means this value
    /// has no such colour, which is the same "the catalogue and the value have
    /// drifted apart" answer [`IndicatorSettings::period`] gives.
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

    /// Writes one named colour. Returns whether this value has it.
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

    /// Rules 4–7 of [`CandlestickIndicatorConfig::normalize`], in order.
    fn normalized(mut self, id: IndicatorId) -> Self {
        // Rule 4: the value's shape must be the one the catalogue declares for
        // this indicator; anything else is replaced by the whole default value.
        if self.shape() != expected_shape(id) {
            return (definition_of(id).default_settings)();
        }
        // Rule 5: a line list never stays empty. The replacement row takes the
        // indicator's default period and the first fixed colour.
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
        // Rule 7: every colour is normalized against the fixed colour of its own
        // slot, so a bad component never poisons equality or painting.
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

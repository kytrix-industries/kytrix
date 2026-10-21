//! The persisted indicator configuration and its normalization.
//!
//! The types here are the disk format: one entry per indicator, in catalogue
//! order, plus the per-indicator settings value. A configuration that came from
//! disk is untrusted — it may be older, hand-edited or written by a build with a
//! different catalogue — so [`CandlestickIndicatorConfig::normalize`] restores
//! the invariants the type documents.

use serde::{Deserialize, Deserializer, Serialize};

use super::{INDICATOR_CATALOG, IndicatorId, default_line_color, definition_of};

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
/// 4. every period is within `1..=500`.
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
                    row.period = row.period.clamp(1, 500);
                }
            }
            Self::Macd {
                short_period,
                long_period,
                signal_period,
                ..
            } => {
                *short_period = (*short_period).clamp(1, 500);
                *long_period = (*long_period).clamp(1, 500);
                *signal_period = (*signal_period).clamp(1, 500);
            }
            Self::Kdj {
                period,
                smoothing_1,
                smoothing_2,
                ..
            } => {
                *period = (*period).clamp(1, 500);
                *smoothing_1 = (*smoothing_1).clamp(1, 500);
                *smoothing_2 = (*smoothing_2).clamp(1, 500);
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

// TODO(§6): `random_line_color` — the colour a line the user just added gets —
// lands together with the panel's "+" button, its first and only caller. The
// design's §10.2 fixes its shape: a random hue with saturation and lightness
// clamped to a readable band, converted through `gpui_kit`'s `hsla`/`Rgba`, and
// stored in `IndicatorLineConfig.color` from then on.

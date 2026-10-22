//! Indicator series computed from the loaded candles.
//!
//! Pure functions with no GPUI dependency: the chart painter builds the
//! [`IndicatorInput`] columns from a snapshot and asks for every enabled
//! indicator's lines in one pass (design §7.1). Values are indexed by the
//! absolute candle index of the whole snapshot, never by a visible-window slot;
//! the legend and the price-bounds fold both rely on that (design §5.6, §6.3).
//!
//! The formulas are the design's §7.2, and the depths come from [`line_depth`]
//! and [`settings_depth`] in `config.rs`: the same function that decides an
//! [`IndicatorLine`]'s display threshold feeds the history prefetch, so the two
//! cannot disagree.

use gpui_kit::Hsla;
use smol_str::{SmolStr, format_smolstr};

use super::{
    CandlestickIndicatorConfig, IndicatorEntry, IndicatorId, IndicatorLineConfig,
    IndicatorSettings, LineColor, default_line_color, line_depth, settings_depth,
};

/// The prices of one candle that the computations read.
///
/// The chart's own [`crate::model::CandlePrices`] resolves a candle's legs;
/// this type is the narrowed contract of the computation: it knows three prices
/// and nothing about listings, quote assets or units.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IndicatorInput {
    pub close: f64,
    pub high: f64,
    pub low: f64,
}

/// How a line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineShape {
    Line,
    Histogram,
}

/// The y range of one sub plot, derived from the visible window.
///
/// It is deliberately not stored on [`IndicatorSeries`]: a range depends on the
/// visible window (design §6.4), while the values are computed once per snapshot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ValueRange {
    pub lowest: f64,
    pub highest: f64,
}

/// One computed line, indexed like the whole snapshot's candle array.
pub(crate) struct IndicatorLine {
    /// The line's name, e.g. `MA5`, `DIF` or `J`.
    pub label: SmolStr,
    /// The colour the line and its legend entry are drawn in.
    pub color: Hsla,
    /// One entry per candle; `None` means the value is not defined there.
    pub values: Vec<Option<f64>>,
    /// The first absolute index whose value is trustworthy.
    ///
    /// The line still has values before this index where the formula defines
    /// one (EMA and MACD do, from the first candle on); the legend and the readout
    /// read no value before it. It is computed per line from that line's own
    /// settings, not from the indicator's longest depth.
    pub readable_from: usize,
    pub shape: LineShape,
}

/// One enabled indicator's computed output.
///
/// It deliberately holds no y-axis range: a range depends on the visible window
/// and is derived every frame (design §6.4), so storing one here would freeze it
/// behind the "computed once per snapshot" cache.
pub(crate) struct IndicatorSeries {
    pub id: IndicatorId,
    /// The formula name shown by a sub-plot legend, e.g. `MACD(12,26,9)`.
    pub legend: SmolStr,
    pub lines: Vec<IndicatorLine>,
    /// The reference lines the sub plot draws at fixed values.
    ///
    /// They depend only on the indicator, not on the visible window, so they
    /// belong to the cached series rather than to a per-frame range.
    pub guides: Vec<f64>,
}

/// Computes every enabled line indicator, in catalogue order.
///
/// Volume is not here: it is a sub-plot indicator whose bars the painter builds
/// from the snapshot's traded sizes directly, and it has no values to compute.
/// `None`-setting indicators are skipped for the same reason.
pub(crate) fn compute_indicators(
    inputs: &[IndicatorInput],
    config: &CandlestickIndicatorConfig,
) -> Vec<IndicatorSeries> {
    config
        .entries
        .iter()
        .filter(|entry| entry.enabled)
        .filter_map(|entry| series_for(inputs, entry))
        .collect()
}

/// One enabled entry's series, or `None` for an indicator with no computed line.
fn series_for(inputs: &[IndicatorInput], entry: &IndicatorEntry) -> Option<IndicatorSeries> {
    match (entry.id, &entry.settings) {
        (IndicatorId::Ma, IndicatorSettings::Lines(lines)) => Some(lines_series(
            inputs,
            entry.id,
            SmolStr::new_static("MA"),
            &lines.rows,
            sma,
            Vec::new(),
        )),
        (IndicatorId::Ema, IndicatorSettings::Lines(lines)) => Some(lines_series(
            inputs,
            entry.id,
            SmolStr::new_static("EMA"),
            &lines.rows,
            ema_line,
            Vec::new(),
        )),
        (IndicatorId::Rsi, IndicatorSettings::Lines(lines)) => Some(lines_series(
            inputs,
            entry.id,
            SmolStr::new_static("RSI"),
            &lines.rows,
            rsi,
            vec![30.0, 70.0],
        )),
        (
            IndicatorId::Macd,
            IndicatorSettings::Macd {
                short_period,
                long_period,
                signal_period,
                dif,
                dea,
                dif_color,
                dea_color,
            },
        ) => Some(macd_series(
            inputs,
            *short_period,
            *long_period,
            *signal_period,
            *dif,
            *dea,
            *dif_color,
            *dea_color,
            settings_depth(entry.id, &entry.settings),
        )),
        (
            IndicatorId::Kdj,
            IndicatorSettings::Kdj {
                period,
                smoothing_1,
                smoothing_2,
                k,
                d,
                j,
                k_color,
                d_color,
                j_color,
            },
        ) => Some(kdj_series(
            inputs,
            *period,
            *smoothing_1,
            *smoothing_2,
            *k,
            *d,
            *j,
            *k_color,
            *d_color,
            *j_color,
            settings_depth(entry.id, &entry.settings),
        )),
        // VOL has no computed line, and a settings value of the wrong shape
        // cannot survive normalization; both are simply not drawn.
        _ => None,
    }
}

/// One `Lines` indicator: one line per configured row.
///
/// The row's period decides both the formula and that line's `readable_from`, so
/// MA 5/10/20 is readable from 4/9/19 rather than from the maximum 19.
fn lines_series(
    inputs: &[IndicatorInput],
    id: IndicatorId,
    legend: SmolStr,
    rows: &[IndicatorLineConfig],
    formula: fn(&[IndicatorInput], u32) -> Vec<Option<f64>>,
    guides: Vec<f64>,
) -> IndicatorSeries {
    let mut series = IndicatorSeries {
        id,
        legend,
        lines: Vec::with_capacity(rows.len()),
        guides,
    };
    for row in rows {
        // The line's name is the indicator's formula name followed by its
        // period (`MA5`, `EMA12`, `RSI6`); taking the prefix from the series
        // legend keeps the two spellings from drifting apart.
        let label = format_smolstr!("{}{}", series.legend, row.period);
        series.lines.push(IndicatorLine {
            label,
            color: Hsla::from(row.color),
            values: formula(inputs, row.period),
            readable_from: line_depth(id, row.period),
            shape: LineShape::Line,
        });
    }
    series
}

fn macd_series(
    inputs: &[IndicatorInput],
    short_period: u32,
    long_period: u32,
    signal_period: u32,
    draw_dif: bool,
    draw_dea: bool,
    dif_color: LineColor,
    dea_color: LineColor,
    readable_from: usize,
) -> IndicatorSeries {
    let closes: Vec<f64> = inputs.iter().map(|input| input.close).collect();
    let short = ema_closes(&closes, short_period);
    let long = ema_closes(&closes, long_period);
    let dif: Vec<f64> = short
        .iter()
        .zip(&long)
        .map(|(short, long)| short - long)
        .collect();
    let dea = ema_closes(&dif, signal_period);
    // The histogram is always drawn: its two switches only choose the DIF and
    // DEA lines. The ×2 is the convention the design fixes (Tongdaxin).
    let histogram = dif
        .iter()
        .zip(&dea)
        .map(|(dif, dea)| Some((dif - dea) * 2.0))
        .collect();

    let mut lines = Vec::with_capacity(3);
    if draw_dif {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("DIF"),
            color: Hsla::from(dif_color),
            values: dif.into_iter().map(Some).collect(),
            readable_from,
            shape: LineShape::Line,
        });
    }
    if draw_dea {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("DEA"),
            color: Hsla::from(dea_color),
            values: dea.into_iter().map(Some).collect(),
            readable_from,
            shape: LineShape::Line,
        });
    }
    lines.push(IndicatorLine {
        label: SmolStr::new_static("MACD"),
        color: Hsla::from(default_line_color(2)),
        values: histogram,
        readable_from,
        shape: LineShape::Histogram,
    });

    IndicatorSeries {
        id: IndicatorId::Macd,
        legend: format_smolstr!("MACD({short_period},{long_period},{signal_period})"),
        lines,
        guides: vec![0.0],
    }
}

fn kdj_series(
    inputs: &[IndicatorInput],
    period: u32,
    smoothing_1: u32,
    smoothing_2: u32,
    draw_k: bool,
    draw_d: bool,
    draw_j: bool,
    k_color: LineColor,
    d_color: LineColor,
    j_color: LineColor,
    readable_from: usize,
) -> IndicatorSeries {
    let (k, d, j) = kdj(inputs, period, smoothing_1, smoothing_2);

    let mut lines = Vec::with_capacity(3);
    if draw_k {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("K"),
            color: Hsla::from(k_color),
            values: k,
            readable_from,
            shape: LineShape::Line,
        });
    }
    if draw_d {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("D"),
            color: Hsla::from(d_color),
            values: d,
            readable_from,
            shape: LineShape::Line,
        });
    }
    if draw_j {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("J"),
            color: Hsla::from(j_color),
            values: j,
            readable_from,
            shape: LineShape::Line,
        });
    }

    IndicatorSeries {
        id: IndicatorId::Kdj,
        legend: format_smolstr!("KDJ({period},{smoothing_1},{smoothing_2})"),
        lines,
        guides: vec![20.0, 80.0],
    }
}

/// Simple moving average over the closes, with a sliding sum.
///
/// The first value is at index `period - 1`; before that there are not enough
/// candles and the entries stay `None`.
fn sma(inputs: &[IndicatorInput], period: u32) -> Vec<Option<f64>> {
    let period = period as usize;
    let mut values = vec![None; inputs.len()];
    if period == 0 {
        return values;
    }
    let mut sum = 0.0;
    for (index, input) in inputs.iter().enumerate() {
        sum += input.close;
        if index >= period {
            sum -= inputs[index - period].close;
        }
        if index + 1 >= period {
            values[index] = Some(sum / period as f64);
        }
    }
    values
}

/// Exponential moving average over the closes, seeded with the first close.
///
/// It has a value from the first candle on; the value only becomes trustworthy
/// after `period - 1` candles, which is [`line_depth`]'s answer rather than a
/// property of this function.
fn ema_line(inputs: &[IndicatorInput], period: u32) -> Vec<Option<f64>> {
    let closes: Vec<f64> = inputs.iter().map(|input| input.close).collect();
    ema_closes(&closes, period).into_iter().map(Some).collect()
}

fn ema_closes(closes: &[f64], period: u32) -> Vec<f64> {
    let alpha = 2.0 / (f64::from(period) + 1.0);
    let mut values = Vec::with_capacity(closes.len());
    for &close in closes {
        let next = match values.last() {
            Some(&previous) => alpha * close + (1.0 - alpha) * previous,
            None => close,
        };
        values.push(next);
    }
    values
}

/// Wilder-smoothed relative strength index.
///
/// The first change needs one candle, so the first value is at index `period`
/// (`period + 1` candles in); everything before it is `None`.
fn rsi(inputs: &[IndicatorInput], period: u32) -> Vec<Option<f64>> {
    let period = period as usize;
    let mut values = vec![None; inputs.len()];
    if period == 0 || inputs.len() <= period {
        return values;
    }

    let mut average_gain = 0.0;
    let mut average_loss = 0.0;
    for index in 1..=period {
        let change = inputs[index].close - inputs[index - 1].close;
        average_gain += change.max(0.0);
        average_loss += (-change).max(0.0);
    }
    average_gain /= period as f64;
    average_loss /= period as f64;
    values[period] = Some(rsi_value(average_gain, average_loss));

    for index in (period + 1)..inputs.len() {
        let change = inputs[index].close - inputs[index - 1].close;
        average_gain = (average_gain * (period - 1) as f64 + change.max(0.0)) / period as f64;
        average_loss = (average_loss * (period - 1) as f64 + (-change).max(0.0)) / period as f64;
        values[index] = Some(rsi_value(average_gain, average_loss));
    }
    values
}

/// A flat market has no gains and no losses; the design fixes its RSI at 50
/// rather than leaving the ratio undefined.
fn rsi_value(average_gain: f64, average_loss: f64) -> f64 {
    let total = average_gain + average_loss;
    if total == 0.0 {
        50.0
    } else {
        100.0 * average_gain / total
    }
}

/// The K / D / J lines of the stochastic oscillator.
///
/// The recursion is seeded with `prev_k = prev_d = 50`, and the first RSV index
/// is smoothed once like every later one. Before that index the outputs are
/// `None` and the seeds are **not** advanced: feeding a `None` into the
/// recursion would make the first real value NaN.
fn kdj(
    inputs: &[IndicatorInput],
    period: u32,
    smoothing_1: u32,
    smoothing_2: u32,
) -> (Vec<Option<f64>>, Vec<Option<f64>>, Vec<Option<f64>>) {
    let len = inputs.len();
    let mut k_values = vec![None; len];
    let mut d_values = vec![None; len];
    let mut j_values = vec![None; len];

    let period = period as usize;
    if period == 0 || len < period {
        return (k_values, d_values, j_values);
    }
    let first = period - 1;
    let smoothing_1 = f64::from(smoothing_1);
    let smoothing_2 = f64::from(smoothing_2);
    let mut previous_k = 50.0;
    let mut previous_d = 50.0;

    for index in first..len {
        let window = &inputs[index + 1 - period..=index];
        let highest = window
            .iter()
            .map(|input| input.high)
            .fold(f64::MIN, f64::max);
        let lowest = window
            .iter()
            .map(|input| input.low)
            .fold(f64::MAX, f64::min);
        // A window that never moved has no range to normalise against; the
        // design fixes its RSV at the neutral 50.
        let rsv = if highest == lowest {
            50.0
        } else {
            (inputs[index].close - lowest) / (highest - lowest) * 100.0
        };
        let k = (previous_k * (smoothing_1 - 1.0) + rsv) / smoothing_1;
        let d = (previous_d * (smoothing_2 - 1.0) + k) / smoothing_2;
        let j = 3.0 * k - 2.0 * d;

        k_values[index] = Some(k);
        d_values[index] = Some(d);
        j_values[index] = Some(j);
        previous_k = k;
        previous_d = d;
    }

    (k_values, d_values, j_values)
}

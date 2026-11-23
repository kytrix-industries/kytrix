use smol_str::{SmolStr, format_smolstr};
use ui_core::paint::color::Color;

use super::{
    CandlestickIndicatorConfig, IndicatorEntry, IndicatorId, IndicatorLineConfig,
    IndicatorSettings, LineColor, default_line_color, line_depth, settings_depth,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct IndicatorInput {
    pub close: f64,
    pub high: f64,
    pub low: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineShape {
    Line,
    Histogram,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ValueRange {
    pub lowest: f64,
    pub highest: f64,
}

pub(crate) struct IndicatorLine {
    pub label: SmolStr,
    pub color: Color,
    pub values: Vec<Option<f64>>,
    pub readable_from: usize,
    pub shape: LineShape,
}

pub(crate) struct IndicatorSeries {
    pub id: IndicatorId,
    pub legend: SmolStr,
    pub lines: Vec<IndicatorLine>,
    pub guides: Vec<f64>,
}

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
        _ => None,
    }
}

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
        let label = format_smolstr!("{}{}", series.legend, row.period);
        series.lines.push(IndicatorLine {
            label,
            color: Color::from(row.color),
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
    let histogram = dif
        .iter()
        .zip(&dea)
        .map(|(dif, dea)| Some((dif - dea) * 2.0))
        .collect();

    let mut lines = Vec::with_capacity(3);
    if draw_dif {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("DIF"),
            color: Color::from(dif_color),
            values: dif.into_iter().map(Some).collect(),
            readable_from,
            shape: LineShape::Line,
        });
    }
    if draw_dea {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("DEA"),
            color: Color::from(dea_color),
            values: dea.into_iter().map(Some).collect(),
            readable_from,
            shape: LineShape::Line,
        });
    }
    lines.push(IndicatorLine {
        label: SmolStr::new_static("MACD"),
        color: Color::from(default_line_color(2)),
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
            color: Color::from(k_color),
            values: k,
            readable_from,
            shape: LineShape::Line,
        });
    }
    if draw_d {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("D"),
            color: Color::from(d_color),
            values: d,
            readable_from,
            shape: LineShape::Line,
        });
    }
    if draw_j {
        lines.push(IndicatorLine {
            label: SmolStr::new_static("J"),
            color: Color::from(j_color),
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

fn rsi_value(average_gain: f64, average_loss: f64) -> f64 {
    let total = average_gain + average_loss;
    if total == 0.0 {
        50.0
    } else {
        100.0 * average_gain / total
    }
}

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

//! Tests for the indicator computation.
//!
//! Every expected number below is hand-computed from the design's §7.2
//! formulas, so a change to a formula fails here rather than on screen.

use gpui_kit::{Hsla, hsla};

use super::{
    CandlestickIndicatorConfig, IndicatorId, IndicatorInput, IndicatorLine, IndicatorLineConfig,
    IndicatorSeries, IndicatorSettings, LineColor, LineListConfig, LineShape, compute_indicators,
    default_line_color, line_depth, settings_depth,
};

fn lines(periods: &[u32]) -> IndicatorSettings {
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

fn lines_with_colors(rows: &[(u32, LineColor)]) -> IndicatorSettings {
    IndicatorSettings::Lines(LineListConfig {
        rows: rows
            .iter()
            .map(|(period, color)| IndicatorLineConfig {
                period: *period,
                color: *color,
            })
            .collect(),
    })
}

/// A configuration with exactly one indicator enabled and its settings replaced.
fn config_with(id: IndicatorId, settings: IndicatorSettings) -> CandlestickIndicatorConfig {
    let mut config = CandlestickIndicatorConfig::default();
    for entry in &mut config.entries {
        entry.enabled = entry.id == id;
        if entry.id == id {
            entry.settings = settings.clone();
        }
    }
    config
}

fn settings_of(config: &CandlestickIndicatorConfig, id: IndicatorId) -> &IndicatorSettings {
    &config
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .expect("the configuration covers the catalogue")
        .settings
}

/// Candles whose high and low are both the close, which is what the averages and
/// RSI read; the KDJ cases use the same shape deliberately.
fn flat_inputs(closes: &[f64]) -> Vec<IndicatorInput> {
    closes
        .iter()
        .map(|close| IndicatorInput {
            close: *close,
            high: *close,
            low: *close,
        })
        .collect()
}

fn series_of(series: &[IndicatorSeries], id: IndicatorId) -> &IndicatorSeries {
    series
        .iter()
        .find(|series| series.id == id)
        .unwrap_or_else(|| panic!("{id:?} series should have been computed"))
}

fn line_of<'a>(series: &'a IndicatorSeries, label: &str) -> &'a IndicatorLine {
    series
        .lines
        .iter()
        .find(|line| line.label == label)
        .unwrap_or_else(|| panic!("{label} line should have been computed"))
}

fn assert_values(label: &str, actual: &[Option<f64>], expected: &[Option<f64>]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{label} has one entry per candle"
    );
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        match (actual, expected) {
            (None, None) => {}
            (Some(actual), Some(expected)) => assert!(
                (actual - expected).abs() < 1e-9,
                "{label}[{index}] is {actual}, expected {expected}"
            ),
            _ => panic!("{label}[{index}] is {actual:?}, expected {expected:?}"),
        }
    }
}

// ① SMA follows the sliding sum: the first value is at n - 1.
#[test]
fn sma_should_follow_the_hand_computed_sliding_average() {
    let config = config_with(IndicatorId::Ma, lines(&[3]));
    let series = compute_indicators(&flat_inputs(&[1.0, 2.0, 3.0, 4.0, 5.0]), &config);
    let line = line_of(series_of(&series, IndicatorId::Ma), "MA3");

    assert_values(
        "MA3",
        &line.values,
        &[None, None, Some(2.0), Some(3.0), Some(4.0)],
    );
    assert_eq!(line.readable_from, 2);
    assert_eq!(line.shape, LineShape::Line);
}

// ① EMA is seeded with the first close and has a value from index 0; its
// display threshold is still n - 1.
#[test]
fn ema_should_be_seeded_with_the_first_close() {
    let config = config_with(IndicatorId::Ema, lines(&[3]));
    let series = compute_indicators(&flat_inputs(&[1.0, 2.0, 3.0, 4.0, 5.0]), &config);
    let line = line_of(series_of(&series, IndicatorId::Ema), "EMA3");

    // alpha = 0.5: 1, 1.5, 2.25, 3.125, 4.0625.
    assert_values(
        "EMA3",
        &line.values,
        &[Some(1.0), Some(1.5), Some(2.25), Some(3.125), Some(4.0625)],
    );
    assert_eq!(line.readable_from, 2);
}

// ① RSI is Wilder-smoothed, and its first value is at index n.
#[test]
fn rsi_should_follow_wilder_smoothing() {
    let config = config_with(IndicatorId::Rsi, lines(&[3]));
    let series = compute_indicators(&flat_inputs(&[1.0, 2.0, 3.0, 2.0, 1.0, 2.0]), &config);
    let line = line_of(series_of(&series, IndicatorId::Rsi), "RSI3");

    // Deltas: +1, +1, -1, -1, +1.
    // i=3: gain 2/3, loss 1/3            -> 200/3
    // i=4: gain 4/9, loss 5/9            -> 400/9
    // i=5: gain 17/27, loss 10/27        -> 1700/27
    assert_values(
        "RSI3",
        &line.values,
        &[
            None,
            None,
            None,
            Some(200.0 / 3.0),
            Some(400.0 / 9.0),
            Some(1700.0 / 27.0),
        ],
    );
    assert_eq!(line.readable_from, 3);
}

// ③ A market that did not move has no gain and no loss, which reads as 50.
#[test]
fn rsi_should_read_fifty_without_any_gain_or_loss() {
    let config = config_with(IndicatorId::Rsi, lines(&[3]));
    let series = compute_indicators(&flat_inputs(&[10.0, 10.0, 10.0, 10.0]), &config);
    let line = line_of(series_of(&series, IndicatorId::Rsi), "RSI3");

    assert_eq!(line.values[3], Some(50.0));
}

// ① MACD's DEA is an EMA of DIF seeded with DIF[0], and the histogram is
// (DIF - DEA) * 2.
#[test]
fn macd_should_use_the_dif_ema_as_its_signal_line() {
    let config = config_with(
        IndicatorId::Macd,
        IndicatorSettings::Macd {
            short_period: 2,
            long_period: 3,
            signal_period: 2,
            dif: true,
            dea: true,
            dif_color: default_line_color(0),
            dea_color: default_line_color(1),
        },
    );
    let series = compute_indicators(&flat_inputs(&[1.0, 2.0, 3.0]), &config);
    let series = series_of(&series, IndicatorId::Macd);

    // EMA2: 1, 5/3, 23/9; EMA3: 1, 3/2, 9/4.
    // DIF: 0, 1/6, 11/36; DEA (EMA of DIF, seed 0): 0, 1/9, 13/54.
    // Histogram: 0, 1/9, 7/54.
    assert_values(
        "DIF",
        &line_of(series, "DIF").values,
        &[Some(0.0), Some(1.0 / 6.0), Some(11.0 / 36.0)],
    );
    assert_values(
        "DEA",
        &line_of(series, "DEA").values,
        &[Some(0.0), Some(1.0 / 9.0), Some(13.0 / 54.0)],
    );
    assert_values(
        "MACD",
        &line_of(series, "MACD").values,
        &[Some(0.0), Some(1.0 / 9.0), Some(7.0 / 54.0)],
    );
    assert_eq!(series.lines.len(), 3);
    assert_eq!(line_of(series, "MACD").shape, LineShape::Histogram);
    assert_eq!(
        line_of(series, "DIF").readable_from,
        settings_depth(IndicatorId::Macd, settings_of(&config, IndicatorId::Macd))
    );
}

// ⑤ A short period longer than the long one is defined and simply reverses the
// lines; nothing panics and every value flips sign.
#[test]
fn macd_should_stay_defined_when_the_short_period_is_longer() {
    let reversed = config_with(
        IndicatorId::Macd,
        IndicatorSettings::Macd {
            short_period: 5,
            long_period: 2,
            signal_period: 3,
            dif: true,
            dea: true,
            dif_color: default_line_color(0),
            dea_color: default_line_color(1),
        },
    );
    let normal = config_with(
        IndicatorId::Macd,
        IndicatorSettings::Macd {
            short_period: 2,
            long_period: 5,
            signal_period: 3,
            dif: true,
            dea: true,
            dif_color: default_line_color(0),
            dea_color: default_line_color(1),
        },
    );
    let inputs = flat_inputs(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    let reversed = compute_indicators(&inputs, &reversed);
    let normal = compute_indicators(&inputs, &normal);
    let reversed = series_of(&reversed, IndicatorId::Macd);
    let normal = series_of(&normal, IndicatorId::Macd);

    for label in ["DIF", "DEA", "MACD"] {
        let reversed = &line_of(reversed, label).values;
        let normal = &line_of(normal, label).values;
        for (index, (reversed, normal)) in reversed.iter().zip(normal).enumerate() {
            let (Some(reversed), Some(normal)) = (reversed, normal) else {
                panic!("{label}[{index}] should be defined for both period orders");
            };
            assert!(
                (*reversed + *normal).abs() < 1e-9,
                "{label}[{index}] should flip sign, got {reversed} and {normal}"
            );
        }
    }
    assert_eq!(line_of(normal, "DIF").readable_from, 4);
}

// ④ A window that never moved has no high-low range, so RSV is the neutral 50.
#[test]
fn kdj_should_read_fifty_when_high_equals_low() {
    let config = config_with(
        IndicatorId::Kdj,
        IndicatorSettings::Kdj {
            period: 3,
            smoothing_1: 3,
            smoothing_2: 3,
            k: true,
            d: true,
            j: true,
            k_color: default_line_color(0),
            d_color: default_line_color(1),
            j_color: default_line_color(2),
        },
    );
    let series = compute_indicators(&flat_inputs(&[5.0, 5.0, 5.0]), &config);
    let series = series_of(&series, IndicatorId::Kdj);

    assert_values("K", &line_of(series, "K").values, &[None, None, Some(50.0)]);
    assert_values("D", &line_of(series, "D").values, &[None, None, Some(50.0)]);
    assert_values("J", &line_of(series, "J").values, &[None, None, Some(50.0)]);
}

// ⑦ The KDJ recursion is seeded with 50 and smooths the first RSV once; three
// candles only pin the seed and that first smoothing.
#[test]
fn kdj_should_seed_its_recursion_at_fifty() {
    let config = config_with(
        IndicatorId::Kdj,
        IndicatorSettings::Kdj {
            period: 3,
            smoothing_1: 2,
            smoothing_2: 2,
            k: true,
            d: true,
            j: true,
            k_color: default_line_color(0),
            d_color: default_line_color(1),
            j_color: default_line_color(2),
        },
    );
    let series = compute_indicators(&flat_inputs(&[10.0, 11.0, 12.0]), &config);
    let series = series_of(&series, IndicatorId::Kdj);

    // RSV[2] = 100; K = (50 * 1 + 100) / 2 = 75; D = (50 * 1 + 75) / 2 = 62.5;
    // J = 3 * 75 - 2 * 62.5 = 100.
    assert_values("K", &line_of(series, "K").values, &[None, None, Some(75.0)]);
    assert_values("D", &line_of(series, "D").values, &[None, None, Some(62.5)]);
    assert_values(
        "J",
        &line_of(series, "J").values,
        &[None, None, Some(100.0)],
    );
}

// ⑦ Five candles prove that the previous K/D are advanced only past the first
// RSV: a recursion that advanced the 50 seeds before `first` would fail here.
#[test]
fn kdj_should_advance_the_previous_values_only_after_the_first_rsv() {
    let config = config_with(
        IndicatorId::Kdj,
        IndicatorSettings::Kdj {
            period: 3,
            smoothing_1: 2,
            smoothing_2: 2,
            k: true,
            d: true,
            j: true,
            k_color: default_line_color(0),
            d_color: default_line_color(1),
            j_color: default_line_color(2),
        },
    );
    let series = compute_indicators(&flat_inputs(&[10.0, 11.0, 12.0, 11.0, 10.0]), &config);
    let series = series_of(&series, IndicatorId::Kdj);

    // RSV: 100, 0, 0.
    // K: 75, 37.5, 18.75; D: 62.5, 50, 34.375; J: 100, 12.5, -12.5.
    assert_values(
        "K",
        &line_of(series, "K").values,
        &[None, None, Some(75.0), Some(37.5), Some(18.75)],
    );
    assert_values(
        "D",
        &line_of(series, "D").values,
        &[None, None, Some(62.5), Some(50.0), Some(34.375)],
    );
    assert_values(
        "J",
        &line_of(series, "J").values,
        &[None, None, Some(100.0), Some(12.5), Some(-12.5)],
    );
    // The design's KDJ range must keep the negative J visible; pinning the
    // value here is what later justifies `lowest < 0` in the sub-plot range.
    assert!(line_of(series, "J").values[4].is_some_and(|j| j < 0.0));
}

// ⑥ A single candle defines only what a seed can produce: EMA and MACD have a
// value at index 0, while SMA, RSI and KDJ need more candles.
#[test]
fn a_single_candle_should_only_define_the_seeded_lines() {
    let mut config = CandlestickIndicatorConfig::default();
    for entry in &mut config.entries {
        entry.enabled = true;
    }
    let series = compute_indicators(&flat_inputs(&[100.0]), &config);

    let ma = series_of(&series, IndicatorId::Ma);
    assert!(ma.lines.iter().all(|line| line.values[0].is_none()));
    let ema = line_of(series_of(&series, IndicatorId::Ema), "EMA12");
    assert_eq!(ema.values[0], Some(100.0));
    let macd = series_of(&series, IndicatorId::Macd);
    for label in ["DIF", "DEA", "MACD"] {
        assert_eq!(line_of(macd, label).values[0], Some(0.0));
    }
    assert!(
        series_of(&series, IndicatorId::Rsi)
            .lines
            .iter()
            .all(|line| line.values[0].is_none())
    );
    assert!(
        series_of(&series, IndicatorId::Kdj)
            .lines
            .iter()
            .all(|line| line.values[0].is_none())
    );
}

// Every line reports the depth of its own period: MA 5/10/20 is readable from
// 4/9/19, not from the indicator's longest depth 19.
#[test]
fn every_line_should_report_the_depth_of_its_own_period() {
    let mut config = CandlestickIndicatorConfig::default();
    for entry in &mut config.entries {
        entry.enabled = matches!(
            entry.id,
            IndicatorId::Ma | IndicatorId::Ema | IndicatorId::Rsi
        );
    }
    let series = compute_indicators(&flat_inputs(&[1.0; 30]), &config);

    let ma = series_of(&series, IndicatorId::Ma);
    let depths: Vec<usize> = ma.lines.iter().map(|line| line.readable_from).collect();
    assert_eq!(depths, [4, 9, 19]);
    assert_eq!(
        depths.iter().copied().max(),
        Some(settings_depth(
            IndicatorId::Ma,
            settings_of(&config, IndicatorId::Ma)
        )),
        "the indicator-level depth is only the maximum over its lines"
    );
    assert_eq!(
        line_of(ma, "MA5").readable_from,
        line_depth(IndicatorId::Ma, 5)
    );
    assert_eq!(
        line_of(ma, "MA20").readable_from,
        line_depth(IndicatorId::Ma, 20)
    );

    let ema = series_of(&series, IndicatorId::Ema);
    assert_eq!(ema.lines[0].readable_from, 11);
    let rsi = series_of(&series, IndicatorId::Rsi);
    assert_eq!(
        rsi.lines
            .iter()
            .map(|line| line.readable_from)
            .collect::<Vec<_>>(),
        [6, 12, 24]
    );
}

// The output follows catalogue order and never contains VOL.
#[test]
fn computed_series_should_follow_catalogue_order_and_skip_volume() {
    let default_only = compute_indicators(
        &flat_inputs(&[1.0; 3]),
        &CandlestickIndicatorConfig::default(),
    );
    assert_eq!(default_only.len(), 1, "only MA is a computed default line");
    assert_eq!(default_only[0].id, IndicatorId::Ma);

    let mut all = CandlestickIndicatorConfig::default();
    for entry in &mut all.entries {
        entry.enabled = true;
    }
    let ids: Vec<IndicatorId> = compute_indicators(&flat_inputs(&[1.0; 3]), &all)
        .iter()
        .map(|series| series.id)
        .collect();
    assert_eq!(
        ids,
        [
            IndicatorId::Ma,
            IndicatorId::Ema,
            IndicatorId::Macd,
            IndicatorId::Rsi,
            IndicatorId::Kdj,
        ]
    );
}

// The sub-plot reference lines are a property of the indicator, not of the
// visible window, so they travel with the cached series.
#[test]
fn sub_plot_series_should_carry_their_reference_lines() {
    let mut config = CandlestickIndicatorConfig::default();
    for entry in &mut config.entries {
        entry.enabled = true;
    }
    let series = compute_indicators(&flat_inputs(&[1.0; 3]), &config);

    assert_eq!(series_of(&series, IndicatorId::Macd).guides, [0.0]);
    assert_eq!(series_of(&series, IndicatorId::Rsi).guides, [30.0, 70.0]);
    assert_eq!(series_of(&series, IndicatorId::Kdj).guides, [20.0, 80.0]);
    assert!(series_of(&series, IndicatorId::Ma).guides.is_empty());
    assert!(series_of(&series, IndicatorId::Ema).guides.is_empty());
}

// The label is the formula name plus the line's period, and the legend keeps
// the indicator's own parameters.
#[test]
fn labels_should_name_the_line_and_legend_should_name_the_formula() {
    let config = config_with(
        IndicatorId::Macd,
        IndicatorSettings::Macd {
            short_period: 12,
            long_period: 26,
            signal_period: 9,
            dif: true,
            dea: true,
            dif_color: default_line_color(0),
            dea_color: default_line_color(1),
        },
    );
    let series = compute_indicators(&flat_inputs(&[1.0; 3]), &config);
    let macd = series_of(&series, IndicatorId::Macd);
    assert_eq!(macd.legend, "MACD(12,26,9)");
    let labels: Vec<&str> = macd.lines.iter().map(|line| line.label.as_str()).collect();
    assert_eq!(labels, ["DIF", "DEA", "MACD"]);

    let config = config_with(IndicatorId::Ma, lines(&[5, 10]));
    let series = compute_indicators(&flat_inputs(&[1.0; 3]), &config);
    assert_eq!(series_of(&series, IndicatorId::Ma).legend, "MA");
    let labels: Vec<&str> = series_of(&series, IndicatorId::Ma)
        .lines
        .iter()
        .map(|line| line.label.as_str())
        .collect();
    assert_eq!(labels, ["MA5", "MA10"]);
}

// MACD's switches choose the two lines only; the histogram is always drawn.
#[test]
fn macd_switches_should_only_choose_the_dif_and_dea_lines() {
    let config = config_with(
        IndicatorId::Macd,
        IndicatorSettings::Macd {
            short_period: 2,
            long_period: 3,
            signal_period: 2,
            dif: false,
            dea: false,
            dif_color: default_line_color(0),
            dea_color: default_line_color(1),
        },
    );
    let series = compute_indicators(&flat_inputs(&[1.0, 2.0, 3.0]), &config);
    let labels: Vec<&str> = series_of(&series, IndicatorId::Macd)
        .lines
        .iter()
        .map(|line| line.label.as_str())
        .collect();

    assert_eq!(labels, ["MACD"]);
}

// The KDJ switches choose their lines one by one.
#[test]
fn kdj_switches_should_choose_their_lines_one_by_one() {
    let config = config_with(
        IndicatorId::Kdj,
        IndicatorSettings::Kdj {
            period: 3,
            smoothing_1: 2,
            smoothing_2: 2,
            k: false,
            d: true,
            j: false,
            k_color: default_line_color(0),
            d_color: default_line_color(1),
            j_color: default_line_color(2),
        },
    );
    let series = compute_indicators(&flat_inputs(&[10.0, 11.0, 12.0]), &config);
    let series = series_of(&series, IndicatorId::Kdj);

    assert_eq!(series.lines.len(), 1);
    assert_eq!(series.legend, "KDJ(3,2,2)");
    assert_eq!(series.lines[0].label, "D");
}

// Switching every KDJ line off is a valid configuration: the series still
// exists (so the sub-plot can be laid out later) but draws no line.
#[test]
fn kdj_with_every_line_switched_off_should_draw_an_empty_series() {
    let config = config_with(
        IndicatorId::Kdj,
        IndicatorSettings::Kdj {
            period: 3,
            smoothing_1: 2,
            smoothing_2: 2,
            k: false,
            d: false,
            j: false,
            k_color: default_line_color(0),
            d_color: default_line_color(1),
            j_color: default_line_color(2),
        },
    );
    let series = compute_indicators(&flat_inputs(&[10.0, 11.0, 12.0]), &config);
    let series = series_of(&series, IndicatorId::Kdj);

    assert!(series.lines.is_empty());
    assert_eq!(series.legend, "KDJ(3,2,2)");
}

// Every computed line carries the colour its settings store. A `Line` line is
// painted in it and its sub-plot legend entry is drawn in it, but not every
// stored colour is drawn: the MACD histogram's bars use the bullish/bearish
// market colours, and the sub-plot legend does not list its `Histogram` line.
// The fixed third colour is therefore only the value the model gives the
// histogram line, not a colour the user sees.
#[test]
fn computed_lines_should_carry_the_configured_colours() {
    fn color(h: f32) -> LineColor {
        LineColor {
            h,
            s: 0.5,
            l: 0.4,
            a: 0.75,
        }
    }

    let (ma_color, ema_color, rsi_color) = (color(0.11), color(0.22), color(0.33));
    let (dif_color, dea_color) = (color(0.44), color(0.55));
    let (k_color, d_color, j_color) = (color(0.66), color(0.77), color(0.88));

    let config = config_with(IndicatorId::Ma, lines_with_colors(&[(3, ma_color)]));
    let series = compute_indicators(&flat_inputs(&[1.0; 5]), &config);
    assert_eq!(
        line_of(series_of(&series, IndicatorId::Ma), "MA3").color,
        hsla(0.11, 0.5, 0.4, 0.75)
    );

    let config = config_with(IndicatorId::Ema, lines_with_colors(&[(3, ema_color)]));
    let series = compute_indicators(&flat_inputs(&[1.0; 5]), &config);
    assert_eq!(
        line_of(series_of(&series, IndicatorId::Ema), "EMA3").color,
        hsla(0.22, 0.5, 0.4, 0.75)
    );

    let config = config_with(IndicatorId::Rsi, lines_with_colors(&[(3, rsi_color)]));
    let series = compute_indicators(&flat_inputs(&[1.0; 5]), &config);
    assert_eq!(
        line_of(series_of(&series, IndicatorId::Rsi), "RSI3").color,
        hsla(0.33, 0.5, 0.4, 0.75)
    );

    let mut config = config_with(
        IndicatorId::Macd,
        IndicatorSettings::Macd {
            short_period: 3,
            long_period: 4,
            signal_period: 2,
            dif: true,
            dea: true,
            dif_color,
            dea_color,
        },
    );
    for entry in &mut config.entries {
        if entry.id == IndicatorId::Kdj {
            entry.enabled = true;
            entry.settings = IndicatorSettings::Kdj {
                period: 3,
                smoothing_1: 2,
                smoothing_2: 2,
                k: true,
                d: true,
                j: true,
                k_color,
                d_color,
                j_color,
            };
        }
    }
    let series = compute_indicators(&flat_inputs(&[1.0; 5]), &config);

    let macd = series_of(&series, IndicatorId::Macd);
    assert_eq!(line_of(macd, "DIF").color, hsla(0.44, 0.5, 0.4, 0.75));
    assert_eq!(line_of(macd, "DEA").color, hsla(0.55, 0.5, 0.4, 0.75));
    assert_eq!(
        line_of(macd, "MACD").color,
        Hsla::from(default_line_color(2)),
        "the histogram line keeps the fixed third colour; neither its bars nor the legend draw it"
    );

    let kdj = series_of(&series, IndicatorId::Kdj);
    assert_eq!(line_of(kdj, "K").color, hsla(0.66, 0.5, 0.4, 0.75));
    assert_eq!(line_of(kdj, "D").color, hsla(0.77, 0.5, 0.4, 0.75));
    assert_eq!(line_of(kdj, "J").color, hsla(0.88, 0.5, 0.4, 0.75));
}

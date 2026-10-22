//! Tests for the indicator configuration and its normalization.

use super::{
    config::{
        CandlestickIndicatorConfig, IndicatorEntry, IndicatorLineConfig, IndicatorSettings,
        LineListConfig, enabled_sub_plot_ids, expected_shape,
    },
    definition::{INDICATOR_CATALOG, IndicatorId, default_line_color},
};
use crate::view::CandlestickConfig;

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

fn entry(id: IndicatorId, enabled: bool, settings: IndicatorSettings) -> IndicatorEntry {
    IndicatorEntry {
        id,
        enabled,
        settings,
    }
}

fn entry_of(config: &CandlestickIndicatorConfig, id: IndicatorId) -> &IndicatorEntry {
    config
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .expect("a normalized configuration covers the catalogue")
}

fn periods(settings: &IndicatorSettings) -> Vec<u32> {
    match settings {
        IndicatorSettings::None => Vec::new(),
        IndicatorSettings::Lines(lines) => lines.rows.iter().map(|row| row.period).collect(),
        IndicatorSettings::Macd {
            short_period,
            long_period,
            signal_period,
            ..
        } => vec![*short_period, *long_period, *signal_period],
        IndicatorSettings::Kdj {
            period,
            smoothing_1,
            smoothing_2,
            ..
        } => vec![*period, *smoothing_1, *smoothing_2],
    }
}

// ① The default configuration holds all four invariants of its type.
#[test]
fn the_default_configuration_should_satisfy_every_invariant() {
    let config = CandlestickIndicatorConfig::default();

    assert_eq!(config.entries.len(), INDICATOR_CATALOG.len());
    for (entry, definition) in config.entries.iter().zip(INDICATOR_CATALOG) {
        assert_eq!(
            entry.id, definition.id,
            "the default configuration follows catalogue order"
        );
        assert_eq!(entry.enabled, definition.enabled_by_default);
        assert_eq!(
            entry.settings.shape(),
            expected_shape(entry.id),
            "{:?} defaults to the wrong settings shape",
            definition.id
        );
        assert!(
            periods(&entry.settings)
                .iter()
                .all(|period| (1..=500).contains(period)),
            "{:?} defaults to a period outside 1..=500",
            definition.id
        );
        if let IndicatorSettings::Lines(lines) = &entry.settings {
            assert!(
                !lines.rows.is_empty(),
                "{:?} defaults to an empty line list",
                definition.id
            );
        }
    }
}

// ② A default configuration survives the JSON round trip unchanged.
#[test]
fn a_configuration_should_round_trip_through_json() {
    let config = CandlestickIndicatorConfig::default();
    let encoded = serde_json::to_string(&config).expect("a default configuration serializes");
    let decoded: CandlestickIndicatorConfig =
        serde_json::from_str(&encoded).expect("the encoded configuration decodes");

    assert_eq!(decoded, config);
}

// ③ Normalization fills missing entries, keeps the first duplicate, repairs a
// settings value of the wrong shape and clamps out-of-range periods item by item.
#[test]
fn normalize_should_fill_reorder_dedupe_and_clamp() {
    let config = CandlestickIndicatorConfig {
        entries: vec![
            entry(IndicatorId::Ema, true, lines(&[0, 999])),
            entry(IndicatorId::Ma, true, lines(&[0])),
            entry(IndicatorId::Ma, false, lines(&[7])),
            entry(IndicatorId::Vol, false, lines(&[5])),
            entry(
                IndicatorId::Macd,
                true,
                IndicatorSettings::Macd {
                    short_period: 0,
                    long_period: 999,
                    signal_period: 501,
                    dif: false,
                    dea: true,
                },
            ),
            entry(
                IndicatorId::Kdj,
                true,
                IndicatorSettings::Kdj {
                    period: 0,
                    smoothing_1: 501,
                    smoothing_2: 999,
                    k: false,
                    d: false,
                    j: false,
                },
            ),
        ],
    };

    let normalized = config.normalize();

    let order: Vec<IndicatorId> = normalized.entries.iter().map(|entry| entry.id).collect();
    assert_eq!(
        order,
        [
            IndicatorId::Ma,
            IndicatorId::Ema,
            IndicatorId::Vol,
            IndicatorId::Macd,
            IndicatorId::Rsi,
            IndicatorId::Kdj,
        ],
        "normalization reorders the entries by the catalogue"
    );

    let vol = entry_of(&normalized, IndicatorId::Vol);
    assert_eq!(
        vol.settings,
        IndicatorSettings::None,
        "a settings value of the wrong shape is replaced by the indicator's default"
    );
    assert!(
        !vol.enabled,
        "repairing the settings value must not reset the enabled flag"
    );

    let ema = entry_of(&normalized, IndicatorId::Ema);
    assert!(ema.enabled);
    assert_eq!(
        periods(&ema.settings),
        [1, 500],
        "out-of-range line periods are clamped item by item"
    );

    let rsi = entry_of(&normalized, IndicatorId::Rsi);
    assert_eq!(rsi.settings, lines(&[6, 12, 24]));
    assert!(
        !rsi.enabled,
        "a missing entry takes the catalogue's enabled state"
    );

    // The first MA wins, so its clamped period is 1 rather than the dropped
    // duplicate's 7.
    let ma = entry_of(&normalized, IndicatorId::Ma);
    assert!(ma.enabled);
    assert_eq!(periods(&ma.settings), [1]);

    let macd = entry_of(&normalized, IndicatorId::Macd);
    assert_eq!(periods(&macd.settings), [1, 500, 500]);
    match &macd.settings {
        IndicatorSettings::Macd { dif, dea, .. } => {
            assert!(!dif);
            assert!(dea);
        }
        other => panic!("MACD kept its settings shape, got {other:?}"),
    }

    let kdj = entry_of(&normalized, IndicatorId::Kdj);
    assert_eq!(periods(&kdj.settings), [1, 500, 500]);
    match &kdj.settings {
        IndicatorSettings::Kdj { k, d, j, .. } => {
            assert!(!k);
            assert!(!d);
            assert!(!j);
        }
        other => panic!("KDJ kept its settings shape, got {other:?}"),
    }
}

// ④ An empty line list is repaired with one row from the indicator's defaults.
#[test]
fn an_empty_line_list_should_be_repaired_with_one_default_row() {
    let empty = || IndicatorSettings::Lines(LineListConfig { rows: Vec::new() });
    let config = CandlestickIndicatorConfig {
        entries: vec![
            entry(IndicatorId::Ma, true, empty()),
            entry(IndicatorId::Ema, true, empty()),
        ],
    };

    let normalized = config.normalize();
    let ma = entry_of(&normalized, IndicatorId::Ma);
    let IndicatorSettings::Lines(repaired) = &ma.settings else {
        panic!("MA kept its line list");
    };

    assert_eq!(repaired.rows.len(), 1);
    assert_eq!(
        repaired.rows[0].period, 5,
        "the repaired row takes MA's first default period"
    );
    assert_eq!(repaired.rows[0].color, default_line_color(0));

    let ema = entry_of(&normalized, IndicatorId::Ema);
    let IndicatorSettings::Lines(repaired) = &ema.settings else {
        panic!("EMA kept its line list");
    };

    assert_eq!(repaired.rows.len(), 1);
    assert_eq!(
        repaired.rows[0].period, 12,
        "the repaired row takes EMA's own first default period, not MA's"
    );
    assert_eq!(repaired.rows[0].color, default_line_color(0));
}

// ⑤ A payload from before the indicator field existed does not decode: the
// field has no `serde(default)`, so it cannot be silently filled in (D8).
#[test]
fn a_payload_without_the_indicator_field_should_fail_to_decode() {
    let payload = r#"{"selection":null,"timeframe":"1m","adjustment":"none"}"#;
    let error = serde_json::from_str::<CandlestickConfig>(payload)
        .expect_err("a payload without `indicator` must not decode");

    let message = error.to_string();
    assert!(
        message.contains("missing field `indicator`"),
        "the failure should name the missing field, got: {message}"
    );
}

// ⑥ `normalize` really runs through `deserialize_with`: a payload with one
// entry decodes into a completed, ordered configuration.
#[test]
fn decoding_a_configuration_should_normalize_the_indicator_field() {
    let payload = r#"{
        "selection": null,
        "timeframe": "1m",
        "adjustment": "none",
        "indicator": {
            "entries": [{
                "id": "ma",
                "enabled": false,
                "settings": {
                    "kind": "lines",
                    "rows": [{"period": 7, "color": [1, 2, 3, 255]}]
                }
            }]
        }
    }"#;

    let decoded: CandlestickConfig =
        serde_json::from_str(payload).expect("the payload decodes into a configuration");
    let encoded = serde_json::to_value(&decoded).expect("the decoded configuration serializes");
    let entries = encoded["indicator"]["entries"]
        .as_array()
        .expect("the entries are an array");

    assert_eq!(
        entries.len(),
        INDICATOR_CATALOG.len(),
        "decoding filled the missing indicators in"
    );
    assert_eq!(entries[0]["id"].as_str(), Some("ma"));
    assert_eq!(entries[0]["enabled"].as_bool(), Some(false));
    assert_eq!(
        entries[0]["settings"]["rows"][0]["period"].as_u64(),
        Some(7)
    );
    assert_eq!(
        entries[2]["id"].as_str(),
        Some("vol"),
        "the decoded entries are in catalogue order"
    );
    assert_eq!(entries[2]["settings"]["kind"].as_str(), Some("none"));
    assert_eq!(
        entries[2]["enabled"].as_bool(),
        Some(true),
        "a filled entry takes the catalogue's default enabled state"
    );
}

// ⑦ `warmup_depth` follows the design's §7.2 table per indicator, and the
// configuration answers with the longest requirement among enabled ones.
#[test]
fn every_indicator_should_report_the_warmup_depth_of_its_formula() {
    let cases: [(IndicatorSettings, IndicatorId, usize); 7] = [
        (IndicatorSettings::None, IndicatorId::Vol, 0),
        (lines(&[5, 10, 20]), IndicatorId::Ma, 19),
        (lines(&[12]), IndicatorId::Ema, 11),
        (
            IndicatorSettings::Macd {
                short_period: 12,
                long_period: 26,
                signal_period: 9,
                dif: true,
                dea: true,
            },
            IndicatorId::Macd,
            25,
        ),
        (
            // The longer average decides, whichever side it is on: a formula
            // that assumed `long` were always the longer one would answer 9.
            IndicatorSettings::Macd {
                short_period: 30,
                long_period: 10,
                signal_period: 9,
                dif: true,
                dea: true,
            },
            IndicatorId::Macd,
            29,
        ),
        (lines(&[6, 12, 24]), IndicatorId::Rsi, 24),
        (
            IndicatorSettings::Kdj {
                period: 9,
                smoothing_1: 3,
                smoothing_2: 3,
                k: true,
                d: true,
                j: true,
            },
            IndicatorId::Kdj,
            12,
        ),
    ];
    for (settings, id, expected) in cases {
        assert_eq!(
            settings.warmup_depth(id),
            expected,
            "{id:?} needs {expected} leading candles"
        );
    }

    // The default configuration enables MA (19) and VOL (0), so it reports 19;
    // an indicator that is switched off does not contribute.
    assert_eq!(CandlestickIndicatorConfig::default().warmup_depth(), 19);

    let mut only_kdj = CandlestickIndicatorConfig::default();
    for entry in &mut only_kdj.entries {
        entry.enabled = entry.id == IndicatorId::Kdj;
    }
    assert_eq!(only_kdj.warmup_depth(), 12);
}

// The sub plots come from the configuration, not from the computed series:
// VOL is enabled by default but has no computed values, so a count derived from
// the series would draw one sub plot too few. Catalogue order is the stacking
// order.
#[test]
fn enabled_sub_plot_ids_should_follow_catalogue_order() {
    assert_eq!(
        enabled_sub_plot_ids(&CandlestickIndicatorConfig::default()),
        [IndicatorId::Vol]
    );

    let mut all_off = CandlestickIndicatorConfig::default();
    for entry in &mut all_off.entries {
        entry.enabled = false;
    }
    assert!(enabled_sub_plot_ids(&all_off).is_empty());
}

#[test]
fn enabled_sub_plot_ids_should_keep_the_catalogue_order_when_several_are_on() {
    let mut config = CandlestickIndicatorConfig::default();
    for entry in &mut config.entries {
        entry.enabled = matches!(
            entry.id,
            IndicatorId::Vol | IndicatorId::Macd | IndicatorId::Rsi
        );
    }

    assert_eq!(
        enabled_sub_plot_ids(&config),
        [IndicatorId::Vol, IndicatorId::Macd, IndicatorId::Rsi],
        "VOL comes before MACD and MACD before RSI because the catalogue does"
    );

    // A main-chart indicator is never a sub plot, even when it is the only one
    // enabled.
    let mut only_ma = CandlestickIndicatorConfig::default();
    for entry in &mut only_ma.entries {
        entry.enabled = entry.id == IndicatorId::Ma;
    }
    assert!(enabled_sub_plot_ids(&only_ma).is_empty());

    // Turning the default VOL off must leave no empty box behind: MACD becomes
    // the first sub plot.
    let mut without_volume = CandlestickIndicatorConfig::default();
    for entry in &mut without_volume.entries {
        entry.enabled = entry.id == IndicatorId::Macd;
    }
    assert_eq!(
        enabled_sub_plot_ids(&without_volume),
        [IndicatorId::Macd],
        "the disabled VOL must not occupy the first sub plot"
    );
}

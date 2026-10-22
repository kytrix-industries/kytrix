//! Tests for the indicator configuration and its normalization.

use gpui_kit::{Hsla, Rgba, hsla};

use super::{
    config::{
        CandlestickIndicatorConfig, IndicatorEntry, IndicatorLineConfig, IndicatorSettings,
        LineColor, LineListConfig, enabled_sub_plot_ids, expected_shape,
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

/// The line list `id` holds, which every line-list indicator must.
fn line_list_of(config: &CandlestickIndicatorConfig, id: IndicatorId) -> &LineListConfig {
    entry_of(config, id)
        .settings
        .lines()
        .unwrap_or_else(|| panic!("{id:?} holds a line list"))
}

/// The eight-bit sRGB bytes `color` quantizes to.
///
/// The old format stored exactly these bytes, so this is how the fixed defaults
/// are compared with the colours the design recorded.
fn srgba_bytes(color: LineColor) -> [u8; 4] {
    let color = Rgba::from(Hsla::from(color));
    [
        (color.r * 255.0).round() as u8,
        (color.g * 255.0).round() as u8,
        (color.b * 255.0).round() as u8,
        (color.a * 255.0).round() as u8,
    ]
}

/// Six clearly distinct colours, deliberately not the theme's six.
fn synthetic_palette() -> [Hsla; 6] {
    [
        hsla(0.0, 0.2, 0.3, 1.0),
        hsla(0.1, 0.3, 0.4, 0.9),
        hsla(0.2, 0.4, 0.5, 0.8),
        hsla(0.3, 0.5, 0.6, 0.7),
        hsla(0.4, 0.6, 0.7, 0.6),
        hsla(0.5, 0.7, 0.8, 0.5),
    ]
}

/// Every colour `settings` stores.
fn colors(settings: &IndicatorSettings) -> Vec<LineColor> {
    match settings {
        IndicatorSettings::None => Vec::new(),
        IndicatorSettings::Lines(lines) => lines.rows.iter().map(|row| row.color).collect(),
        IndicatorSettings::Macd {
            dif_color,
            dea_color,
            ..
        } => vec![*dif_color, *dea_color],
        IndicatorSettings::Kdj {
            k_color,
            d_color,
            j_color,
            ..
        } => vec![*k_color, *d_color, *j_color],
    }
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

// ① The default configuration holds all five invariants of its type.
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
        assert!(
            colors(&entry.settings).iter().all(|color| {
                [color.h, color.s, color.l, color.a]
                    .iter()
                    .all(|component| component.is_finite() && (0.0..=1.0).contains(component))
            }),
            "{:?} defaults to a colour outside 0..=1",
            definition.id
        );
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
                    dif_color: default_line_color(0),
                    dea_color: default_line_color(1),
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
                    k_color: default_line_color(0),
                    d_color: default_line_color(1),
                    j_color: default_line_color(2),
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
                    "rows": [{
                        "period": 7,
                        "color": {"h": 0.1, "s": 0.5, "l": 0.5, "a": 1.0}
                    }]
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
                dif_color: default_line_color(0),
                dea_color: default_line_color(1),
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
                dif_color: default_line_color(0),
                dea_color: default_line_color(1),
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
                k_color: default_line_color(0),
                d_color: default_line_color(1),
                j_color: default_line_color(2),
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

#[test]
fn the_default_line_colours_should_be_the_shipped_amber_violet_and_cyan() {
    assert_eq!(srgba_bytes(default_line_color(0)), [0xE8, 0xA8, 0x3C, 0xFF]);
    assert_eq!(srgba_bytes(default_line_color(1)), [0x8A, 0x6C, 0xE8, 0xFF]);
    assert_eq!(srgba_bytes(default_line_color(2)), [0x3C, 0xB8, 0xC8, 0xFF]);
    assert_eq!(
        default_line_color(3),
        default_line_color(0),
        "the three fixed colours cycle"
    );
}

#[test]
fn adding_a_line_should_take_the_next_palette_colour() {
    let palette = synthetic_palette();
    let mut config = CandlestickIndicatorConfig::default();

    // MA starts with three rows and EMA with one; the next colour is the row
    // count modulo the palette length.
    assert!(config.add_line(IndicatorId::Ma, &palette));
    assert!(config.add_line(IndicatorId::Ema, &palette));
    assert!(config.add_line(IndicatorId::Ma, &palette));
    assert!(config.add_line(IndicatorId::Ma, &palette));
    assert!(config.add_line(IndicatorId::Ma, &palette));

    let ma = line_list_of(&config, IndicatorId::Ma);
    assert_eq!(ma.rows.len(), 7);
    assert_eq!(ma.rows[3].color, LineColor::from(palette[3]));
    assert_eq!(
        ma.rows[4].color,
        LineColor::from(palette[4]),
        "a second addition takes the colour after the first"
    );
    assert_eq!(ma.rows[5].color, LineColor::from(palette[5]));
    assert_eq!(
        ma.rows[6].color,
        LineColor::from(palette[0]),
        "the palette wraps after its last colour"
    );
    assert_eq!(
        periods(&entry_of(&config, IndicatorId::Ma).settings),
        [5, 10, 20, 5, 5, 5, 5],
        "a new line starts at MA's own default period"
    );

    let ema = line_list_of(&config, IndicatorId::Ema);
    assert_eq!(ema.rows.len(), 2);
    assert_eq!(ema.rows[1].color, LineColor::from(palette[1]));
    assert_eq!(
        periods(&entry_of(&config, IndicatorId::Ema).settings),
        [12, 12],
        "a new line starts at EMA's own default period, not MA's"
    );
}

#[test]
fn adding_a_line_should_be_deterministic() {
    let palette = synthetic_palette();
    let mut first = CandlestickIndicatorConfig::default();
    let mut second = CandlestickIndicatorConfig::default();

    first.add_line(IndicatorId::Ma, &palette);
    second.add_line(IndicatorId::Ma, &palette);

    assert_eq!(
        first, second,
        "the same palette produces the same line, with no RNG left"
    );
}

#[test]
fn line_colours_should_normalize_their_components() {
    let mut config = CandlestickIndicatorConfig::default();
    let ma = config
        .entries
        .iter_mut()
        .find(|entry| entry.id == IndicatorId::Ma)
        .expect("the catalogue always has MA");
    let IndicatorSettings::Lines(lines) = &mut ma.settings else {
        panic!("MA is a line list");
    };
    lines.rows[0].color = LineColor {
        h: 1.5,
        s: -0.2,
        l: 2.0,
        a: 1.2,
    };
    lines.rows[1].color = LineColor {
        h: f32::NAN,
        s: 0.5,
        l: 0.5,
        a: 1.0,
    };
    lines.rows[2].color = LineColor {
        h: 0.5,
        s: 0.5,
        l: 0.5,
        a: f32::NAN,
    };
    let ema = config
        .entries
        .iter_mut()
        .find(|entry| entry.id == IndicatorId::Ema)
        .expect("the catalogue always has EMA");
    let IndicatorSettings::Lines(lines) = &mut ema.settings else {
        panic!("EMA is a line list");
    };
    lines.rows[0].color = LineColor {
        h: 0.25,
        s: 0.5,
        l: 0.4,
        a: 0.75,
    };

    let normalized = config.normalize();
    let lines = line_list_of(&normalized, IndicatorId::Ma);

    assert_eq!(
        lines.rows[0].color,
        LineColor {
            h: 0.5,
            s: 0.0,
            l: 1.0,
            a: 1.0,
        },
        "a finite hue wraps and the other components clamp"
    );
    assert_eq!(
        lines.rows[1].color,
        default_line_color(1),
        "a non-finite hue resets the whole colour to its slot's default"
    );
    assert_eq!(
        lines.rows[2].color,
        default_line_color(2),
        "a non-finite alpha also resets the whole colour to its slot's default"
    );
    assert_eq!(
        line_list_of(&normalized, IndicatorId::Ema).rows[0].color,
        LineColor {
            h: 0.25,
            s: 0.5,
            l: 0.4,
            a: 0.75,
        },
        "a valid colour is kept, alpha included"
    );
}

#[test]
fn macd_and_kdj_colours_should_normalize_with_their_own_fallbacks() {
    let mut config = CandlestickIndicatorConfig::default();
    for entry in &mut config.entries {
        match entry.id {
            IndicatorId::Macd => {
                entry.settings = IndicatorSettings::Macd {
                    short_period: 12,
                    long_period: 26,
                    signal_period: 9,
                    dif: true,
                    dea: true,
                    dif_color: LineColor {
                        h: f32::INFINITY,
                        s: 0.5,
                        l: 0.5,
                        a: 1.0,
                    },
                    dea_color: LineColor {
                        h: -0.25,
                        s: f32::NAN,
                        l: 0.5,
                        a: 1.0,
                    },
                };
            }
            IndicatorId::Kdj => {
                entry.settings = IndicatorSettings::Kdj {
                    period: 9,
                    smoothing_1: 3,
                    smoothing_2: 3,
                    k: true,
                    d: true,
                    j: true,
                    k_color: LineColor {
                        h: 0.5,
                        s: 0.5,
                        l: f32::NAN,
                        a: 1.0,
                    },
                    d_color: LineColor {
                        h: 0.5,
                        s: 0.5,
                        l: 0.5,
                        a: f32::NAN,
                    },
                    j_color: LineColor {
                        h: f32::NAN,
                        s: 0.5,
                        l: 0.5,
                        a: 1.0,
                    },
                };
            }
            _ => {}
        }
    }

    let normalized = config.normalize();

    let IndicatorSettings::Macd {
        dif_color,
        dea_color,
        ..
    } = &entry_of(&normalized, IndicatorId::Macd).settings
    else {
        panic!("MACD keeps its settings shape");
    };
    assert_eq!(*dif_color, default_line_color(0));
    assert_eq!(
        *dea_color,
        default_line_color(1),
        "DEA falls back to its own slot, not DIF's"
    );

    let IndicatorSettings::Kdj {
        k_color,
        d_color,
        j_color,
        ..
    } = &entry_of(&normalized, IndicatorId::Kdj).settings
    else {
        panic!("KDJ keeps its settings shape");
    };
    assert_eq!(
        *k_color,
        default_line_color(0),
        "K falls back to its own slot"
    );
    assert_eq!(
        *d_color,
        default_line_color(1),
        "D falls back to its own slot"
    );
    assert_eq!(
        *j_color,
        default_line_color(2),
        "J falls back to its own slot"
    );
}

#[test]
fn the_legacy_srgb_colour_array_should_not_decode() {
    assert!(
        serde_json::from_str::<IndicatorLineConfig>(r#"{"period":5,"color":[1,2,3,255]}"#).is_err(),
        "the sRGB array shape is gone, so a stored configuration falls back to the defaults"
    );
}

#[test]
fn macd_and_kdj_defaults_should_use_the_fixed_line_colours() {
    let config = CandlestickIndicatorConfig::default();

    let IndicatorSettings::Macd {
        dif_color,
        dea_color,
        ..
    } = &entry_of(&config, IndicatorId::Macd).settings
    else {
        panic!("MACD's default is a MACD value");
    };
    assert_eq!(*dif_color, default_line_color(0));
    assert_eq!(*dea_color, default_line_color(1));

    let IndicatorSettings::Kdj {
        k_color,
        d_color,
        j_color,
        ..
    } = &entry_of(&config, IndicatorId::Kdj).settings
    else {
        panic!("KDJ's default is a KDJ value");
    };
    assert_eq!(*k_color, default_line_color(0));
    assert_eq!(*d_color, default_line_color(1));
    assert_eq!(*j_color, default_line_color(2));
}

#[test]
fn line_colour_conversions_should_preserve_alpha() {
    assert_eq!(
        LineColor::from(hsla(0.25, 0.5, 0.5, 0.75)),
        LineColor {
            h: 0.25,
            s: 0.5,
            l: 0.5,
            a: 0.75,
        }
    );
    assert_eq!(
        Hsla::from(LineColor {
            h: 0.25,
            s: 0.5,
            l: 0.5,
            a: 0.75,
        }),
        hsla(0.25, 0.5, 0.5, 0.75)
    );
}

#[test]
fn a_persisted_line_colour_should_keep_its_alpha() {
    let line = IndicatorLineConfig {
        period: 5,
        color: LineColor {
            h: 0.25,
            s: 0.5,
            l: 0.4,
            a: 0.75,
        },
    };
    let encoded = serde_json::to_string(&line).expect("a line colour serializes");
    let decoded: IndicatorLineConfig =
        serde_json::from_str(&encoded).expect("the encoded line decodes");

    assert_eq!(decoded, line);
}

#[test]
fn a_line_colour_map_should_ignore_unknown_fields() {
    let decoded: LineColor =
        serde_json::from_str(r#"{"h":0.25,"s":0.5,"l":0.4,"a":0.75,"extra":1}"#)
            .expect("an unknown field does not invalidate the colour");

    assert_eq!(
        decoded,
        LineColor {
            h: 0.25,
            s: 0.5,
            l: 0.4,
            a: 0.75,
        }
    );
}

#[test]
fn a_line_colour_map_should_require_every_component() {
    assert!(
        serde_json::from_str::<LineColor>(r#"{"h":0.25,"s":0.5,"l":0.4}"#).is_err(),
        "a missing alpha must not default"
    );
}

#[test]
fn a_line_colour_map_should_reject_a_duplicate_component() {
    assert!(
        serde_json::from_str::<LineColor>(r#"{"h":0.25,"h":0.5,"s":0.5,"l":0.4,"a":0.75}"#)
            .is_err(),
        "a duplicate component is ambiguous"
    );
}

#[test]
fn a_stored_configuration_with_the_legacy_colour_array_should_not_decode() {
    let payload = r#"{
        "selection": null,
        "timeframe": "1m",
        "adjustment": "none",
        "indicator": {
            "entries": [{
                "id": "ma",
                "enabled": true,
                "settings": {
                    "kind": "lines",
                    "rows": [{"period": 5, "color": [1, 2, 3, 255]}]
                }
            }]
        }
    }"#;

    assert!(
        serde_json::from_str::<CandlestickConfig>(payload).is_err(),
        "an old colour shape fails the whole decode, so the stored configuration resets to defaults"
    );
}

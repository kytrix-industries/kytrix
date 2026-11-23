use super::{
    config::{SettingsShape, expected_shape},
    definition::{
        INDICATOR_CATALOG, IndicatorId, IndicatorPane, SettingControl, SettingKey, SettingRow,
    },
};
use crate::chart::geometry::SUB_PLOT_INLINE;

const CATALOGUE_ORDER: [IndicatorId; 6] = [
    IndicatorId::Ma,
    IndicatorId::Ema,
    IndicatorId::Vol,
    IndicatorId::Macd,
    IndicatorId::Rsi,
    IndicatorId::Kdj,
];

fn catalogue_order() -> Vec<IndicatorId> {
    INDICATOR_CATALOG
        .iter()
        .map(|definition| definition.id)
        .collect()
}

fn rows_of(shape: SettingsShape) -> &'static [SettingKey] {
    match shape {
        SettingsShape::None => &[],
        SettingsShape::Lines => &[SettingKey::Lines],
        SettingsShape::Macd => &[
            SettingKey::ShortPeriod,
            SettingKey::LongPeriod,
            SettingKey::SignalPeriod,
            SettingKey::Dif,
            SettingKey::Dea,
        ],
        SettingsShape::Kdj => &[
            SettingKey::Period,
            SettingKey::Smoothing1,
            SettingKey::Smoothing2,
            SettingKey::K,
            SettingKey::D,
            SettingKey::J,
        ],
    }
}

fn declared_keys(rows: &[SettingRow]) -> Vec<SettingKey> {
    rows.iter().map(|row| row.key).collect()
}

fn catalog_keys() -> Vec<&'static str> {
    let mut keys: Vec<_> = INDICATOR_CATALOG
        .iter()
        .flat_map(|definition| {
            std::iter::once(definition.name_key)
                .chain(definition.rows.iter().map(|row| row.label_key))
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

#[test]
fn every_indicator_should_appear_exactly_once_in_catalogue_order() {
    assert_eq!(
        catalogue_order(),
        CATALOGUE_ORDER,
        "the catalogue must declare every indicator exactly once, in panel order"
    );
}

#[test]
fn every_indicators_rows_should_edit_the_shape_its_defaults_hold() {
    for definition in INDICATOR_CATALOG {
        let shape = expected_shape(definition.id);
        assert_eq!(
            (definition.default_settings)().shape(),
            shape,
            "{:?} defaults to the wrong settings shape",
            definition.id
        );
        assert_eq!(
            declared_keys(definition.rows),
            rows_of(shape),
            "{:?} declares rows that edit another settings shape",
            definition.id
        );
        for (index, row) in definition.rows.iter().enumerate() {
            assert!(
                definition.rows[..index]
                    .iter()
                    .all(|earlier| earlier.label_key != row.label_key),
                "{:?} declares two rows labelled {}",
                definition.id,
                row.label_key
            );
        }
    }
}

#[test]
fn every_period_row_should_declare_a_non_empty_range() {
    for definition in INDICATOR_CATALOG {
        for row in definition.rows {
            if let SettingControl::Period { min, max } = row.control {
                assert!(
                    min < max,
                    "{:?}::{:?} declares {min}..={max}, which accepts no period",
                    definition.id,
                    row.key
                );
            }
        }
    }
}

#[test]
fn every_line_list_row_should_require_at_least_one_line() {
    for definition in INDICATOR_CATALOG {
        for row in definition.rows {
            if let SettingControl::LineList { min_rows } = row.control {
                assert!(
                    min_rows >= 1,
                    "{:?}::{:?} allows an empty line list, which its settings value cannot hold",
                    definition.id,
                    row.key
                );
            }
        }
    }
}

#[test]
fn every_catalog_key_should_be_defined_and_non_empty_in_every_language() {
    let i18n = i18n::embedded::load().expect("the embedded locales should load");

    for language in i18n.languages() {
        let keys = i18n
            .keys(language)
            .expect("an embedded language exposes its keys")
            .collect::<Vec<_>>();
        for key in catalog_keys() {
            assert!(keys.contains(&key), "{language} does not define {key}");
            assert!(
                !i18n.translate(language, key).is_empty(),
                "{language} renders {key} empty"
            );
        }
    }
}

#[test]
fn the_sub_plot_indicators_should_fit_the_inline_capacity() {
    let sub_plots = INDICATOR_CATALOG
        .iter()
        .filter(|definition| definition.pane == IndicatorPane::Sub)
        .count();

    assert!(
        sub_plots <= SUB_PLOT_INLINE,
        "{sub_plots} sub-plot indicators do not fit the {SUB_PLOT_INLINE} inline layout slots"
    );
}

#[test]
fn every_period_row_should_use_the_bounds_the_message_names() {
    let period_rows: Vec<_> = INDICATOR_CATALOG
        .iter()
        .flat_map(|definition| {
            definition
                .rows
                .iter()
                .filter(|row| matches!(row.control, SettingControl::Period { .. }))
                .map(|row| (definition.id, row.key))
        })
        .collect();
    assert_eq!(
        period_rows,
        [
            (IndicatorId::Macd, SettingKey::ShortPeriod),
            (IndicatorId::Macd, SettingKey::LongPeriod),
            (IndicatorId::Macd, SettingKey::SignalPeriod),
            (IndicatorId::Kdj, SettingKey::Period),
            (IndicatorId::Kdj, SettingKey::Smoothing1),
            (IndicatorId::Kdj, SettingKey::Smoothing2),
        ],
        "the catalogue no longer declares the six rows the period_invalid message is for"
    );

    for definition in INDICATOR_CATALOG {
        for row in definition.rows {
            if let SettingControl::Period { min, max } = row.control {
                assert_eq!(
                    (min, max),
                    (1, 500),
                    "{}::{:?} changes the bounds the period_invalid message names",
                    definition.name_key,
                    row.key
                );
            }
        }
    }
}

#[test]
fn every_switch_row_should_name_the_colour_it_controls() {
    let mut switch_rows = Vec::new();
    for definition in INDICATOR_CATALOG {
        for row in definition.rows {
            match row.control {
                SettingControl::Switch => {
                    switch_rows.push((definition.id, row.key, row.color_key));
                }
                _ => assert!(
                    row.color_key.is_none(),
                    "{:?}::{:?} is not a switch row but names a colour",
                    definition.id,
                    row.key
                ),
            }
        }
    }
    assert_eq!(
        switch_rows,
        [
            (
                IndicatorId::Macd,
                SettingKey::Dif,
                Some(SettingKey::DifColor)
            ),
            (
                IndicatorId::Macd,
                SettingKey::Dea,
                Some(SettingKey::DeaColor)
            ),
            (IndicatorId::Kdj, SettingKey::K, Some(SettingKey::KColor)),
            (IndicatorId::Kdj, SettingKey::D, Some(SettingKey::DColor)),
            (IndicatorId::Kdj, SettingKey::J, Some(SettingKey::JColor)),
        ],
        "a switch row names the colour of the line it switches, one pair per row"
    );
}

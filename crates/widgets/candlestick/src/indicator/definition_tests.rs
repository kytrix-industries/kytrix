//! The catalogue's own invariants: its order, the settings each indicator
//! declares, the default value it is completed with, the i18n keys it names and
//! the period bounds the `period_invalid` message repeats.

use super::{
    config::{SettingsShape, expected_shape},
    definition::{
        INDICATOR_CATALOG, IndicatorId, IndicatorPane, SettingControl, SettingKey, SettingRow,
    },
};
use crate::chart::geometry::SUB_PLOT_INLINE;

/// Every indicator, in the order both the panel and the sub plots rely on.
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

/// The settings rows a value of `shape` is edited through, in the order its
/// fields are declared.
///
/// This is the other half of the correspondence `expected_shape` states: that
/// function says which variant an indicator holds, this one says which rows edit
/// it. `Lines` is one row because a line list edits every line through the same
/// control; the two struct variants name their fields one by one.
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

/// Every i18n key the catalogue names: each indicator's title and each of its
/// rows' labels, sorted and deduplicated.
///
/// The three line-list indicators share `…indicator.lines`, so the same key is
/// collected more than once.
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

// ① Each `IndicatorId` appears exactly once, in catalogue order.
#[test]
fn every_indicator_should_appear_exactly_once_in_catalogue_order() {
    assert_eq!(
        catalogue_order(),
        CATALOGUE_ORDER,
        "the catalogue must declare every indicator exactly once, in panel order"
    );
}

// ② The rows an indicator declares are the settings its default value holds:
// the same shape `expected_shape` names, and the very keys that shape's fields
// are. A catalogue that says "Lines" while its default is MACD's is caught here
// rather than by `normalize` quietly repairing it when a configuration is
// decoded.
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
        // Every row's label is part of the element id its panel row is drawn
        // under, so two rows sharing one would be two rows the window cannot
        // tell apart.
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

// ③ Every `Period` control declares a range with room in it.
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

// ④ A line list always asks for at least the one line its value has to hold.
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

// ⑤ Every key the catalogue names is defined by every embedded language and
// renders to something. A key no locale defines renders as the empty string —
// a blank title or label in the panel rather than an error anywhere — so this
// is the only place the omission would surface.
#[test]
fn every_catalog_key_should_be_defined_and_non_empty_in_every_language() {
    let i18n = i18n::global::embedded::load().expect("the embedded locales should load");

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

// ⑥ The sub-plot indicators must fit the inline capacity `ChartLayout` stores
// them in; a seventh one without raising `SUB_PLOT_INLINE` would silently move
// the boxes to the heap.
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

// ⑦ Every `Period` row declares the range `period_invalid` names in words. The
// bounds come from the catalogue while the message is fixed text, so only a
// test can tie the two together: changing one without the other would leave the
// panel telling the user a range it does not accept.
#[test]
fn every_period_row_should_use_the_bounds_the_message_names() {
    // `if let` alone would let a row that stopped being a `Period` control drop
    // out of the check without failing it, so pin the six rows the design
    // declares: MACD's three periods and KDJ's period and two smoothings.
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

// ⑧ Only a switch row names a colour, and each one names the colour of the line
// it switches. The pair is what puts the picker on that row's right and what the
// picker writes when the user chooses a colour, so a swapped DIF/DEA pair would
// edit the wrong line — which is exactly what this test pins.
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

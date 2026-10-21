//! The catalogue's own invariants: its order, the settings each indicator
//! declares, and the default value it is completed with.
//!
//! Two of the design's checks are deliberately not here: the name keys being
//! present in both languages (§9.2) and the period bounds matching the
//! `period_invalid` message (§9.1). Both need data the i18n stage adds, and the
//! note at the end of this file points at them.

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

// ⑤ 名称 key 在两种语言都非空 —— 见 implementation-plan §9.2
// ⑦ Period 的边界与 period_invalid 文案一致 —— 见 implementation-plan §9.1

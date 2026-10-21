//! The catalogue's own invariants: its order and the default settings value each
//! indicator is completed with.
//!
//! The catalogue's display data (`rows`, `label_key`, `name_key`) is not
//! declared yet, so the tests that read it belong to the stages that first read
//! it — see the note at the end of this file.

use super::{
    config::expected_shape,
    definition::{INDICATOR_CATALOG, IndicatorId, IndicatorPane},
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

// ① Each `IndicatorId` appears exactly once, in catalogue order.
#[test]
fn every_indicator_should_appear_exactly_once_in_catalogue_order() {
    assert_eq!(
        catalogue_order(),
        CATALOGUE_ORDER,
        "the catalogue must declare every indicator exactly once, in panel order"
    );
}

// ② Every indicator's default settings value holds the shape `expected_shape`
// declares for it. The catalogue's rows are checked against that same answer in
// §6, where they are declared.
#[test]
fn every_indicator_should_default_to_its_declared_shape() {
    for definition in INDICATOR_CATALOG {
        assert_eq!(
            (definition.default_settings)().shape(),
            expected_shape(definition.id),
            "{:?} defaults to the wrong settings shape",
            definition.id
        );
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

// ③ Period 控件的 min < max 与 ④ LineList 的 min_rows >= 1（以及 ② 中 rows 的
//    SettingKey 一致性）—— 在 §6 随 rows/control 一起补
// ⑤ 名称 key 在两种语言都非空 —— 见 implementation-plan §9.2
// ⑦ Period 的边界与 period_invalid 文案一致 —— 见 implementation-plan §9.1

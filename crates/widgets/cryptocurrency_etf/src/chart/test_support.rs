use std::{rc::Rc, sync::Arc};

use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use time::{Date, Month};
use ui_core::math::{LogicalRect, px};

use super::{axes::ChartLayout, data::PreparedData};
use crate::model::{
    CryptocurrencyEtfAsset, CryptocurrencyEtfDataset, CryptocurrencyEtfDay,
    CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow,
};

pub(crate) const FUNDS: [&str; 2] = ["AAA", "BBB"];

pub(crate) fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

pub(crate) fn english() -> Translate {
    translate(LanguageCode::En)
}

pub(crate) fn dataset(flows: &[f64]) -> Arc<CryptocurrencyEtfDataset> {
    dataset_rows(&flows.iter().map(|flow| [*flow, 0.0]).collect::<Vec<_>>())
}

pub(crate) fn dataset_rows(rows: &[[f64; 2]]) -> Arc<CryptocurrencyEtfDataset> {
    let first = Date::from_calendar_date(2024, Month::January, 1)
        .expect("the fixture start should be valid")
        .to_julian_day();
    let mut net_assets = 0.0;
    let days = rows
        .iter()
        .enumerate()
        .map(|(index, flows)| {
            let total_flow = flows[0] + flows[1];
            net_assets += total_flow;
            CryptocurrencyEtfDay {
                date: Date::from_julian_day(first + i32::try_from(index).unwrap_or(i32::MAX))
                    .expect("fixture date should be valid"),
                fund_flows: Arc::from(*flows),
                total_flow,
                net_assets,
            }
        })
        .collect::<Vec<_>>();
    Arc::new(CryptocurrencyEtfDataset {
        asset: CryptocurrencyEtfAsset::Btc,
        funds: Arc::from(FUNDS.map(SmolStr::new_static)),
        days: days.into(),
    })
}

pub(crate) fn prepared(
    translate: &Translate,
    flows: &[f64],
    selection: CryptocurrencyEtfFundSelection,
    window: CryptocurrencyEtfWindow,
) -> PreparedData {
    PreparedData::new(translate, dataset(flows), selection, window)
}

pub(crate) fn prepared_all(translate: &Translate, flows: &[f64]) -> PreparedData {
    prepared(
        translate,
        flows,
        CryptocurrencyEtfFundSelection::All,
        CryptocurrencyEtfWindow::All,
    )
}

pub(crate) fn layout(width: f32, height: f32) -> ChartLayout {
    ChartLayout::new(
        LogicalRect::new(px(0.0), px(0.0), px(width), px(height)),
        px(40.0),
        Some(px(60.0)),
    )
}

pub(crate) fn bounds(width: f32, height: f32) -> LogicalRect {
    LogicalRect::new(px(0.0), px(0.0), px(width), px(height))
}

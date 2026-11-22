//! The fixtures every chart test file shares.
//!
//! The chart's data is derived, so every test that needs a chart starts from the same
//! thing: a history of a few days with a known flow, resolved into the view a frame
//! draws. Building it here keeps the tests about what they assert rather than about how
//! a `PreparedData` is assembled.

use std::{rc::Rc, sync::Arc};

use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use time::{Date, Month};
use ui_core::math::Rect;

use super::{axes::ChartLayout, data::PreparedData};
use crate::model::{
    CryptocurrencyEtfAsset, CryptocurrencyEtfDataset, CryptocurrencyEtfDay,
    CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow,
};

/// The funds a fixture asset has, in the order its rows carry them.
pub(crate) const FUNDS: [&str; 2] = ["AAA", "BBB"];

/// The translator the product ships, which is what these tests read.
///
/// A chart is resolved against a `Translate` rather than against an application
/// global, so a test needs the catalogue and a language — the same two the composition
/// root injects.
pub(crate) fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

/// The product catalogue in English, which is what most of these tests assert.
pub(crate) fn english() -> Translate {
    translate(LanguageCode::En)
}

/// A history of one fund per day, oldest first, with the given daily flows.
///
/// Net assets are accumulated the way the model accumulates them, so a fixture is the
/// same shape as an answer.
pub(crate) fn dataset(flows: &[f64]) -> Arc<CryptocurrencyEtfDataset> {
    dataset_rows(&flows.iter().map(|flow| [*flow, 0.0]).collect::<Vec<_>>())
}

/// A history whose every fund has its own flow on every day, which is what a test needs
/// to read one fund out of a complex.
pub(crate) fn dataset_rows(rows: &[[f64; 2]]) -> Arc<CryptocurrencyEtfDataset> {
    // Julian days rather than ordinals of one year: a fixture is allowed to be longer
    // than a year, and an ordinal is not.
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

/// A history of one asset, resolved for one view.
pub(crate) fn prepared(
    translate: &Translate,
    flows: &[f64],
    selection: CryptocurrencyEtfFundSelection,
    window: CryptocurrencyEtfWindow,
) -> PreparedData {
    PreparedData::new(translate, dataset(flows), selection, window)
}

/// The whole complex of a history, which is the view every chart opens with.
pub(crate) fn prepared_all(translate: &Translate, flows: &[f64]) -> PreparedData {
    prepared(
        translate,
        flows,
        CryptocurrencyEtfFundSelection::All,
        CryptocurrencyEtfWindow::All,
    )
}

/// A plot laid out in a chart of this size.
pub(crate) fn layout(width: f32, height: f32) -> ChartLayout {
    ChartLayout::new(Rect::new(0.0, 0.0, width, height), 40.0, Some(60.0))
}

/// The box a frame of a chart of this size is drawn in.
pub(crate) fn bounds(width: f32, height: f32) -> Rect {
    Rect::new(0.0, 0.0, width, height)
}

//! The fixtures three chart test files share.
//!
//! The chart's data is derived, so every test that needs a chart starts from the
//! same thing: a history of a few days with a known flow, resolved into the view a
//! frame draws. Building it here keeps the tests about what they assert rather than
//! about how a `PreparedData` is assembled.

use std::sync::Arc;

use gpui_kit::{App, Bounds, Pixels, point, px, size};
use smol_str::SmolStr;
use time::{Date, Month};

use super::{axes::ChartLayout, data::PreparedData};
use crate::model::{EtfAsset, EtfDataset, EtfDay, EtfFundSelection, EtfWindow};

/// The funds a fixture asset has, in the order its rows carry them.
pub(crate) const FUNDS: [&str; 2] = ["AAA", "BBB"];

/// A history of one fund per day, oldest first, with the given daily flows.
///
/// Net assets are accumulated the way the model accumulates them, so a fixture is
/// the same shape as an answer.
pub(crate) fn dataset(flows: &[f64]) -> Arc<EtfDataset> {
    // Julian days rather than ordinals of one year: a fixture is allowed to be
    // longer than a year, and an ordinal is not.
    let first = Date::from_calendar_date(2024, Month::January, 1)
        .expect("the fixture start should be valid")
        .to_julian_day();
    let mut net_assets = 0.0;
    let days = flows
        .iter()
        .enumerate()
        .map(|(index, flow)| {
            net_assets += flow;
            EtfDay {
                date: Date::from_julian_day(first + i32::try_from(index).unwrap_or(i32::MAX))
                    .expect("fixture date should be valid"),
                fund_flows: Arc::from([*flow, 0.0]),
                total_flow: *flow,
                net_assets,
            }
        })
        .collect::<Vec<_>>();
    Arc::new(EtfDataset {
        asset: EtfAsset::Btc,
        funds: Arc::from(FUNDS.map(SmolStr::new_static)),
        days: days.into(),
    })
}

/// A history of one asset, resolved for one view.
pub(crate) fn prepared(
    cx: &App,
    flows: &[f64],
    selection: EtfFundSelection,
    window: EtfWindow,
) -> PreparedData {
    PreparedData::new(cx, dataset(flows), selection, window)
}

/// The whole complex of a history, which is the view every chart opens with.
pub(crate) fn prepared_all(cx: &App, flows: &[f64]) -> PreparedData {
    prepared(cx, flows, EtfFundSelection::All, EtfWindow::All)
}

/// A plot laid out in a chart of this size.
pub(crate) fn layout(width: f32, height: f32) -> ChartLayout {
    ChartLayout::new(
        Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(height))),
        px(40.0),
        Some(px(60.0)),
    )
}

/// The box a frame of a chart of this size is drawn in.
pub(crate) fn bounds(width: f32, height: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(height)))
}

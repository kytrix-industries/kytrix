//! The resolved view one chart draws: which days a window keeps, what each of them
//! says, and what the pane's own row shows.

use gpui_kit::{App, TestAppContext};
use i18n::language::codes::LanguageCode;
use smol_str::SmolStr;

use super::{
    data::{Metrics, PreparedData, PreparedPoint},
    test_support::{FUNDS, prepared, prepared_all},
};
use crate::model::{CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow};

/// Installs the translator and hands the test an application to resolve a view in.
fn with_locales(cx: &mut TestAppContext, test: impl FnOnce(&mut App)) {
    cx.update(|cx| {
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        test(cx);
    });
}

/// The readings the row shows for a view, which is what the pane draws above the
/// chart.
fn metrics(data: &PreparedData, hover: Option<usize>) -> Metrics {
    data.metrics(hover)
        .expect("a window with days has a reading")
}

/// One line of a tooltip, as it is drawn: the caption the tooltip gives the day's
/// flow, and then the amount the caption belongs to. The two are one reading drawn
/// in two colours, so a test that wants to read it has to put them back together.
fn flow_tooltip(point: &PreparedPoint) -> String {
    format!(
        "{}{}",
        point.flow_tooltip_caption.as_ref(),
        point.flow_label.as_ref()
    )
}

#[gpui_kit::test]
fn a_window_of_days_should_be_resolved_into_points_and_two_axes(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1_000.0, -500.0, 2_000.0]);

        assert_eq!(data.points.len(), 3);
        assert!(data.show_net_assets, "the whole complex reports its assets");
        assert!(data.net_assets.is_some());
        assert_eq!(
            data.points[0].header_date.as_ref(),
            "2024-01-01",
            "a tooltip names the whole trading day"
        );
        assert_eq!(data.points[0].date_label, "01-01");
        assert_eq!(data.points[0].flow, 1_000.0);
        assert_eq!(data.points[0].flow_label.as_ref(), "+$1.00B");
        assert_eq!(
            data.points[0].net_assets_label.as_ref(),
            "$1.00B",
            "net assets after the first day are the day's own flow"
        );
        assert_eq!(
            data.points[2].net_assets_label.as_ref(),
            "$2.50B",
            "and accumulate over the whole history"
        );
        assert_eq!(
            flow_tooltip(&data.points[0]),
            "Daily net inflow (M): +$1.00B",
            "a tooltip names the direction the day went and the amount it moved"
        );
        assert_eq!(
            data.points[0].net_assets_tooltip.as_ref(),
            "Total net assets (M): $1.00B"
        );
    });
}

/// Both axes say what they count and what unit their numbers are read in: the numbers
/// themselves are bare, so nothing else on the chart could say it.
#[gpui_kit::test]
fn the_axes_should_name_the_reading_and_the_unit_it_is_measured_in(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1_000.0]);

        assert_eq!(
            data.left_axis_title.as_ref(),
            "Daily net inflow/outflow (M)"
        );
        assert_eq!(data.right_axis_title.as_ref(), "Total net assets (M)");
    });
}

#[gpui_kit::test]
fn the_caption_should_follow_the_direction_the_flow_went(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1_000.0, -500.0, 0.0]);

        assert_eq!(data.points[0].flow_caption.as_ref(), "ETF net inflow");
        assert_eq!(data.points[1].flow_caption.as_ref(), "ETF net outflow");
        assert_eq!(
            data.points[2].flow_caption.as_ref(),
            "ETF net flow",
            "a day that moved nothing has no direction to name"
        );
    });
}

#[gpui_kit::test]
fn the_tooltip_should_name_the_direction_the_flow_went(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1_000.0, -500.0, 0.0]);

        assert_eq!(
            flow_tooltip(&data.points[0]),
            "Daily net inflow (M): +$1.00B"
        );
        assert_eq!(
            flow_tooltip(&data.points[1]),
            "Daily net outflow (M): -$500.00M"
        );
        assert_eq!(
            flow_tooltip(&data.points[2]),
            "Daily net flow (M): $0.00M",
            "a day that moved nothing has no direction to name"
        );
    });
}

#[gpui_kit::test]
fn a_named_fund_should_have_a_flow_but_no_assets_of_its_own(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let fund = CryptocurrencyEtfFundSelection::Named(SmolStr::new_static(FUNDS[0]));
        let data = prepared(cx, &[1_000.0, 2_000.0], fund, CryptocurrencyEtfWindow::All);

        assert!(!data.show_net_assets);
        assert!(data.net_assets.is_none(), "one fund has no level to plot");
        assert_eq!(data.points.len(), 2);
        assert_eq!(data.points[0].net_assets_label.as_ref(), "--");
        assert_eq!(data.points[0].net_assets_tooltip.as_ref(), "--");
        assert_eq!(
            data.points[0].flow_caption.as_ref(),
            &format!("{} net inflow", FUNDS[0])
        );
        assert_eq!(
            flow_tooltip(&data.points[0]),
            "Daily net inflow (M): +$1.00B",
            "a tooltip names the day's flow, not the series: the pane's own controls already say which series is drawn"
        );
    });
}

#[gpui_kit::test]
fn a_fund_the_asset_does_not_have_should_resolve_to_no_days_at_all(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let missing = CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("NOPE"));
        let data = prepared(
            cx,
            &[1_000.0, 2_000.0],
            missing,
            CryptocurrencyEtfWindow::All,
        );

        assert!(
            data.points.is_empty(),
            "a fund that does not exist has no flow to draw, not the total under its name"
        );
        assert!(data.net_assets.is_none());
        assert!(data.metrics(None).is_none());
    });
}

#[gpui_kit::test]
fn a_window_should_keep_the_most_recent_days(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let flows = vec![1.0; 35];
        let all = prepared_all(cx, &flows);
        let month = prepared(
            cx,
            &flows,
            CryptocurrencyEtfFundSelection::All,
            CryptocurrencyEtfWindow::Days30,
        );

        assert_eq!(all.points.len(), 35);
        assert_eq!(month.points.len(), 30);
        assert_eq!(
            month.points[0].date_label, all.points[5].date_label,
            "the window is the tail of the same history"
        );
        assert_eq!(
            month.points[0].net_assets_label, all.points[5].net_assets_label,
            "and the levels it shows are still the history's own"
        );
    });
}

#[gpui_kit::test]
fn a_window_that_did_not_move_should_still_have_an_axis_with_room(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1_000.0, 1_000.0, 1_000.0]);
        let scale = data
            .net_assets
            .as_ref()
            .expect("the whole complex has a net-asset axis");

        assert!(scale.domain.maximum > scale.domain.minimum);
        assert!(
            scale.ticks.len() >= 2,
            "a flat history still gets grid lines to read it against"
        );
    });
}

#[gpui_kit::test]
fn the_row_should_follow_the_pointer_and_fall_back_to_the_last_day(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let data = prepared_all(cx, &[1_000.0, -500.0, 2_000.0]);

        let last = metrics(&data, None);
        assert_eq!(last.date.as_ref(), "2024-01-03");
        assert_eq!(last.flow.as_ref(), "+$2.00B");
        assert_eq!(last.net_assets.as_deref(), Some("$2.50B"));

        let first = metrics(&data, Some(0));
        assert_eq!(first.date.as_ref(), "2024-01-01");
        assert_eq!(first.flow.as_ref(), "+$1.00B");
        assert_eq!(first.caption.as_ref(), "ETF net inflow");
        assert_eq!(first.flow_value, 1_000.0, "the row colours by the number");

        let out_of_range = metrics(&data, Some(99));
        assert_eq!(
            out_of_range.date, last.date,
            "a hover past the window is the last day's reading, not a gap"
        );
    });
}

#[gpui_kit::test]
fn a_rows_net_assets_should_be_absent_for_a_single_fund(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let fund = CryptocurrencyEtfFundSelection::Named(SmolStr::new_static(FUNDS[1]));
        let data = prepared(cx, &[1_000.0], fund, CryptocurrencyEtfWindow::All);

        assert!(metrics(&data, None).net_assets.is_none());
    });
}

#[gpui_kit::test]
fn the_labels_should_be_the_ones_of_the_language_the_view_was_resolved_in(cx: &mut TestAppContext) {
    with_locales(cx, |cx| {
        let english = prepared_all(cx, &[1_000.0]);
        assert_eq!(english.locale, LanguageCode::En);
        assert_eq!(english.points[0].flow_caption.as_ref(), "ETF net inflow");

        cx.set_global(LanguageCode::ZhCn);
        let chinese = prepared_all(cx, &[1_000.0]);

        assert_eq!(chinese.locale, LanguageCode::ZhCn);
        assert_eq!(chinese.points[0].flow_caption.as_ref(), "ETF 净流入");
        assert_eq!(
            flow_tooltip(&chinese.points[0]),
            "当日净流入（百万）：+$1.00B",
            "the caption is written in the language the view was resolved in, separator included"
        );
        assert_eq!(
            chinese.points[0].net_assets_tooltip.as_ref(),
            "总净资产（百万）：$1.00B"
        );
        assert_eq!(chinese.left_axis_title.as_ref(), "当日净流入/流出（百万）");
        assert_eq!(chinese.right_axis_title.as_ref(), "总净资产（百万）");
        assert_eq!(
            english.locale,
            LanguageCode::En,
            "the view that was already resolved keeps the language it was written in"
        );
    });
}

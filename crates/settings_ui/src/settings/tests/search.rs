//! The tests of the search: what it admits, what it keeps, and what it answers.

use app_config::settings::Config;
use gpui_kit::{Modifiers, SharedString, TestAppContext, px};

use super::test_support::*;

/// The clear control is the field's own, and clearing the field is what brings the whole
/// sidebar back: the field and the answer are one thing, so emptying one empties the other.
#[gpui_kit::test]
fn the_field_should_clear_the_search_it_was_filtered_by(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);

    let search = cx
        .debug_bounds("SETTINGS_SEARCH")
        .expect("the search field should be drawn");
    cx.simulate_click(search.center(), Modifiers::none());
    cx.simulate_input("proxy");
    cx.run_until_parked();

    assert!(
        !dialog.read_with(cx, |dialog, _| dialog.filter.is_all()),
        "typing into the field should narrow the sidebar"
    );

    // The control is drawn inside the field, at the end of the row the field fills.
    cx.simulate_click(
        gpui_kit::point(search.right() - px(18.0), search.center().y),
        Modifiers::none(),
    );
    cx.run_until_parked();

    assert!(
        dialog.read_with(cx, |dialog, _| dialog.filter.is_all()),
        "clearing the field should admit everything again"
    );
}

/// The directory row is reachable by what it configures, even though the page has no fields.
#[gpui_kit::test]
fn the_translation_page_should_be_reachable_by_its_directory_setting(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    panel.update(cx, |dialog, cx| {
        dialog.set_query("model directory".into(), cx);
    });
    settle(cx);

    assert_eq!(current_page(&panel, cx), "ai.translation");
}

/// A query keeps the heading of a group a page under it matches in: the rows are the pages,
/// and a page drawn without the heading over it would be a page whose group the search
/// left behind.
#[gpui_kit::test]
fn a_query_should_keep_the_group_of_a_page_it_matches(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    panel.update(cx, |dialog, cx| dialog.set_query("a".into(), cx));
    settle(cx);

    assert_eq!(
        current_page(&panel, cx),
        "appearance",
        "the query admits the page that is drawn, so the page should not have moved"
    );
    let narrowed = labels(&panel, cx);
    assert!(
        narrowed.contains(&SharedString::new_static("Market participants")),
        "the query should keep the heading it matches in: {narrowed:?}"
    );
    assert!(
        narrowed.contains(&SharedString::new_static("OKX")),
        "the pages the query admits should be listed under the heading: {narrowed:?}"
    );
}

/// A page the query does not admit is a page that is no longer drawn, so what is drawn
/// moves to the first page the query does admit.
#[gpui_kit::test]
fn a_query_should_move_the_page_to_one_it_admits(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);

    select(&dialog, "market_participants.okx", cx);
    dialog.update(cx, |dialog, cx| dialog.set_query("proxy".into(), cx));

    dialog.read_with(cx, |dialog, _| {
        assert!(
            !dialog.filter.admits_page("market_participants.okx"),
            "the query should not admit the page that was selected"
        );
        assert!(
            dialog.filter.admits_page(&dialog.current_page),
            "the page that is drawn must be one the query admits"
        );
    });
}

/// A page whose own label the query does not match but one of whose fields it does is a
/// page the query reaches: it is listed in the sidebar, opened as the page that is drawn,
/// and only the fields that matched are drawn on it.
#[gpui_kit::test]
fn a_query_that_matches_a_field_should_open_the_page_that_holds_it(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    panel.update(cx, |dialog, cx| dialog.set_query("api_key".into(), cx));
    settle(cx);

    assert_eq!(
        current_page(&panel, cx),
        "market_participants.binance",
        "the page that holds the field should be the one the query opens"
    );
    let narrowed = labels(&panel, cx);
    assert!(
        narrowed.contains(&SharedString::new_static("Market participants")),
        "the group that holds the page should be kept: {narrowed:?}"
    );
    assert!(
        narrowed.contains(&SharedString::new_static("Binance"))
            && narrowed.contains(&SharedString::new_static("OKX")),
        "every page a matching field sits on should be listed: {narrowed:?}"
    );
    assert!(
        !narrowed.contains(&SharedString::new_static("Appearance")),
        "a page with no matching label or field should not be listed: {narrowed:?}"
    );
    assert!(
        cx.debug_bounds("SETTINGS_FIELD_market_participants.binance.spot.spot_api_key")
            .is_some(),
        "the field the query matched should be drawn on the page it opened"
    );
    assert!(
        cx.debug_bounds("SETTINGS_FIELD_market_participants.binance.spot.spot_private_key")
            .is_none(),
        "a field the query did not match should not be drawn on a page reached by a field"
    );
}

/// A query that matches nothing is answered in both columns: the sidebar has no row left
/// and the page that was open is not drawn behind it, so the two do not disagree the way
/// they would if the page stayed selected while the sidebar went empty.
#[gpui_kit::test]
fn a_query_that_matches_nothing_should_say_so_in_both_columns(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "proxy-management", cx);
    panel.update(cx, |dialog, cx| {
        dialog.set_query("555".into(), cx);
    });
    settle(cx);

    panel.read_with(cx, |dialog, _| {
        assert!(
            dialog.filter.is_nothing(),
            "the query should admit nothing at all"
        );
    });
    assert!(
        labels(&panel, cx).is_empty(),
        "the sidebar should draw no row for a query that admits nothing"
    );
    assert!(
        cx.debug_bounds("SETTINGS_SIDEBAR_NO_MATCHES").is_some(),
        "the sidebar column should say that nothing matched"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE_NO_MATCHES").is_some(),
        "the page column should say that nothing matched"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PROXY_PAGE").is_none(),
        "the page the query took away must not be drawn behind the empty sidebar"
    );

    panel.update(cx, |dialog, cx| dialog.set_query("".into(), cx));
    settle(cx);

    assert!(
        cx.debug_bounds("SETTINGS_SIDEBAR_NO_MATCHES").is_none()
            && cx.debug_bounds("SETTINGS_PAGE_NO_MATCHES").is_none(),
        "clearing the query should take the message away"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PROXY_PAGE").is_some(),
        "clearing the query should bring back the page that was open before it"
    );
}

/// Clearing a query brings the whole sidebar back, and with it the page that was drawn.
#[gpui_kit::test]
fn clearing_a_query_should_admit_everything_again(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);

    dialog.update(cx, |dialog, cx| {
        dialog.set_query("appearance".into(), cx);
        dialog.set_query("".into(), cx);
    });

    dialog.read_with(cx, |dialog, _| {
        assert!(dialog.filter.is_all());
    });
}

//! The tests of the search: what it admits, what it keeps, and what it answers.

use crate::{
    pages::registry,
    search::SearchFilter,
    settings_tests::test_support::{Harness, configured_settings},
};

/// A query keeps the heading of a group a page under it matches in: the rows are the pages, and a
/// page drawn without the heading over it would be a page whose group the search left behind.
#[test]
fn a_query_should_keep_the_group_of_a_page_it_matches() {
    let mut harness = Harness::with_config(configured_settings());
    harness.settle(2);

    harness.set_query("a");
    assert_eq!(
        harness.page().current_page(),
        "appearance",
        "the query admits the page that is drawn, so the page should not have moved"
    );
    let narrowed = harness.labels();
    assert!(
        narrowed.iter().any(|label| label == "Market participants"),
        "the query should keep the heading it matches in: {narrowed:?}"
    );
    assert!(
        narrowed.iter().any(|label| label == "OKX"),
        "the pages the query admits should be listed under the heading: {narrowed:?}"
    );
}

/// A page the query does not admit is a page that is no longer drawn, so what is drawn moves to the
/// first page the query does admit.
#[test]
fn a_query_should_move_the_page_to_one_it_admits() {
    let mut harness = Harness::with_config(configured_settings());
    harness.settle(2);
    harness.select_page("market_participants.okx");
    assert_eq!(harness.page().current_page(), "market_participants.okx");

    harness.set_query("proxy");

    let filter = harness.page().filter_for_test();
    assert!(
        !filter.admits_page("market_participants.okx"),
        "the query should not admit the page that was selected"
    );
    assert!(
        filter.admits_page(harness.page().current_page().as_ref()),
        "the page that is drawn must be one the query admits"
    );
}

/// A query that matches nothing is answered in both columns: the sidebar has no row left and the
/// page that was open is not drawn behind it, so the two do not disagree the way they would if the
/// page stayed selected while the sidebar went empty.
#[test]
fn a_query_that_matches_nothing_should_say_so_in_both_columns() {
    let mut harness = Harness::with_config(configured_settings());
    harness.settle(2);
    harness.select_page("server-management");

    harness.set_query("555");

    assert!(
        harness.page().filter_for_test().is_nothing(),
        "the query should admit nothing at all"
    );
    assert!(
        harness.find("SETTINGS_SIDEBAR_NO_MATCHES").is_some(),
        "the sidebar column should say that nothing matched"
    );
    assert!(
        harness.find("SETTINGS_PAGE_NO_MATCHES").is_some(),
        "the page column should say that nothing matched"
    );
    assert!(
        harness.find("SETTINGS_PAGE_CONTENT").is_none(),
        "the page the query took away must not be drawn behind the empty sidebar"
    );

    harness.set_query("");

    assert!(
        harness.find("SETTINGS_SIDEBAR_NO_MATCHES").is_none()
            && harness.find("SETTINGS_PAGE_NO_MATCHES").is_none(),
        "clearing the query should take the message away"
    );
    assert!(
        harness.find("SETTINGS_PAGE_CONTENT").is_some()
            || harness.find("SETTINGS_CONTENT").is_some(),
        "clearing the query should bring back the page that was open before it"
    );
}

/// Clearing a query brings the whole sidebar back, and with it the page that was drawn.
#[test]
fn clearing_a_query_should_admit_everything_again() {
    let mut harness = Harness::new();
    harness.settle(2);

    harness.set_query("appearance");
    assert!(
        !matches!(harness.page().filter_for_test(), SearchFilter::All),
        "typing into the field should narrow the sidebar"
    );
    harness.set_query("");
    assert!(
        matches!(harness.page().filter_for_test(), SearchFilter::All),
        "clearing the field should admit everything again"
    );
}

/// A page is reached by its own label in the language the interface is drawn in.
#[test]
fn a_page_should_be_reachable_by_its_localized_label() {
    let mut harness = Harness::new();
    harness.settle(2);

    harness.set_query("appearance");

    assert!(
        harness.page().filter_for_test().admits_page("appearance"),
        "the page's own label should admit it"
    );
    assert_eq!(harness.page().current_page(), "appearance");
}

/// The sidebar's rows are the pages the query admits, and the page behind them is one of them.
#[test]
fn the_sidebar_should_draw_only_the_pages_the_query_admits() {
    let mut harness = Harness::new();
    harness.settle(2);

    harness.set_query("translation");

    let filter = harness.page().filter_for_test();
    for page in registry::pages() {
        let id = page.id();
        let drawn = harness.find(&format!("SETTINGS_ENTRY_{id}")).is_some();
        assert_eq!(
            drawn,
            filter.admits_page(id.as_ref())
                || page
                    .search(&harness.services().translate)
                    .fields
                    .iter()
                    .any(|field| filter.admits_field(id.as_ref(), field.id.as_ref())),
            "the sidebar row of {id} should be drawn exactly when the query reaches its page"
        );
    }
}

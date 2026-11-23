//! The tests of the pages that are a list of setting rows: the about page, the data formatting
//! page and the configure page.
//!
//! A page is driven through the shell the application draws it in: the harness opens the page,
//! presses the controls it built and asserts what the settings received and what the page shows.

use app_config::{network::ProxyRoute, time_zone::DataTimeZone};
use i18n::t_smol;
use market_participants_connector::venue::definition::supported_venues;
use market_participants_model::identifier::VenueId;
use ui_core::event::{Action, KeyCode};
use widget::venue::VenueDisplay;

use crate::settings_tests::test_support::{Harness, configured_settings};

/// Chooses the choice at `index` in one open select.
///
/// The choice is made with the keyboard: the panel highlights the chosen line when it opens, so
/// `Home` and then one `↓` per line above the one wanted reaches the wanted line without depending
/// on where the panel happened to lay its rows out.
fn choose(harness: &mut Harness, key: &str, index: usize) {
    harness.click_node(&format!("{key}:trigger"));
    harness.settle(2);
    harness.press(KeyCode::Home);
    for _ in 0..index {
        harness.press(KeyCode::ArrowDown);
    }
    harness.press(KeyCode::Enter);
    harness.settle(2);
}

/// The select key of a row on the current page.
fn select_key(page: &str, suffix: &str) -> String {
    format!("settings.test.{page}.{suffix}")
}

/// The action that pressing one select's trigger dispatches.
fn trigger_action(key: &str) -> Action {
    Action::Custom(format!("{key}.trigger").into())
}

/// The data formatting page draws its one setting, named the way the catalogue names it.
#[test]
fn the_data_format_page_should_draw_the_time_zone_row() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("data-format");
    harness.settle(1);

    assert_eq!(harness.page().current_page(), "data-format");
    let field = harness
        .field("data-format.offset")
        .expect("the time-zone row should be drawn");
    let label = t_smol!(
        harness.services().translate,
        "settings_ui.data_format.time_zone"
    );
    let description = t_smol!(
        harness.services().translate,
        "settings_ui.data_format.description"
    );
    assert_eq!(harness.page().query(), "");
    assert_eq!(harness.field_label("data-format.offset"), label);
    assert!(
        harness.has_text(field, description.as_str()),
        "the row should explain what the setting changes: {}",
        harness.text_of(field)
    );
}

/// Choosing a time zone in the select stores the offset the choice names.
#[test]
fn choosing_a_time_zone_should_store_it() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("data-format");
    harness.settle(1);

    // The first choice is the westernmost offset the setting offers.
    choose(&mut harness, &select_key("data-format", "offset"), 0);

    let chosen = DataTimeZone::from_hours(DataTimeZone::MIN_HOURS)
        .expect("the westernmost offset is one the setting offers");
    assert_eq!(
        harness.config().data_time_zone,
        chosen,
        "the choice should be stored as the data time zone"
    );
    assert_eq!(
        harness.writes().len(),
        1,
        "one choice stores one configuration"
    );
    assert_eq!(
        harness.writes()[0].data_time_zone,
        chosen,
        "the stored configuration carries the chosen offset"
    );
    let field = harness
        .field("data-format.offset")
        .expect("the row should still be drawn");
    assert!(
        harness.has_text(field, chosen.label().as_str()),
        "the control should show the chosen offset: {}",
        harness.text_of(field)
    );
}

/// A time zone written by another subsystem is what the page's control shows on the frame that
/// follows, and the page writes nothing when it adopts it.
#[test]
fn a_time_zone_written_elsewhere_should_be_drawn_by_the_page() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("data-format");
    harness.settle(1);

    let written = DataTimeZone::from_hours(8).expect("UTC+8 is one the setting offers");
    harness.update_config(|config| config.data_time_zone = written);
    harness.settle(2);

    assert_eq!(harness.config().data_time_zone, written);
    assert!(
        harness.writes().is_empty(),
        "adopting a setting written elsewhere is not a write of it"
    );
    let field = harness
        .field("data-format.offset")
        .expect("the row should still be drawn");
    assert!(
        harness.has_text(field, written.label().as_str()),
        "the control should follow the stored offset: {}",
        harness.text_of(field)
    );
}

/// A query that names the time-zone setting reaches the page through that setting and keeps the
/// row it matched; clearing the query brings the whole page back.
#[test]
fn a_query_for_the_time_zone_should_open_the_page_and_keep_the_row() {
    let mut harness = Harness::new();
    harness.settle(3);

    harness.set_query("timezone");

    assert_eq!(
        harness.page().current_page(),
        "data-format",
        "the page that holds the matched setting should be the one the query opens"
    );
    let filter = harness.page().filter_for_test();
    assert!(
        filter.admits_field("data-format", "data-format.offset"),
        "the offset row's own text should hold the word"
    );
    assert!(
        !filter.admits_page("data-format"),
        "the page's own label does not hold the word, so the row is what matched"
    );
    assert!(
        harness.field("data-format.offset").is_some(),
        "the row the query matched should be drawn on the page it opened"
    );

    harness.set_query("");
    assert!(
        harness.page().filter_for_test().is_all(),
        "clearing the query should admit everything again"
    );
    assert!(
        harness.field("data-format.offset").is_some(),
        "clearing the query should keep the row drawn"
    );
}

/// The about page draws the version of the application that is running.
#[test]
fn the_about_page_should_draw_the_version() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("about");
    harness.settle(1);

    assert_eq!(harness.page().current_page(), "about");
    let field = harness
        .field("about.version")
        .expect("the version row should be drawn");
    let label = t_smol!(harness.services().translate, "settings_ui.about.version");
    assert_eq!(harness.field_label("about.version"), label);
    assert!(
        harness.has_text(field, env!("CARGO_PKG_VERSION")),
        "the row should draw the running version: {}",
        harness.text_of(field)
    );
    assert!(
        harness.find("SETTINGS_CONTROL_about.version").is_some(),
        "the value is drawn in the row's control column"
    );
}

/// The about page is reachable by its name, and the query draws the row it holds.
#[test]
fn a_query_for_the_about_page_should_reach_it() {
    let mut harness = Harness::new();
    harness.settle(3);

    harness.set_query("about");

    assert_eq!(
        harness.page().current_page(),
        "about",
        "the page named by the query should be opened"
    );
    assert!(
        harness.field("about.version").is_some(),
        "a page admitted by its own label draws its settings"
    );
}

/// The configure page draws one row for the default line, one for every venue, and one for every
/// group of Kytrix services.
#[test]
fn the_configure_page_should_draw_every_group() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("configure");
    harness.settle(1);

    assert_eq!(harness.page().current_page(), "configure");
    let default_label = t_smol!(
        harness.services().translate,
        "settings_ui.proxies.default_proxy"
    );
    assert_eq!(harness.field_label("proxy.default-line"), default_label);
    for venue in supported_venues() {
        let field = format!("configure.{}", venue.as_str());
        assert!(
            harness.field(&field).is_some(),
            "the configure page should draw the {field} line"
        );
        assert_eq!(
            harness.field_label(&field),
            venue.display_string(&harness.services().translate)
        );
    }
    for (field, label_key) in [
        ("configure.kytrix.cloud", "settings_ui.proxies.kytrix_cloud"),
        (
            "configure.kytrix.updates",
            "settings_ui.proxies.app_updates",
        ),
        (
            "configure.ai.models",
            "settings_ui.proxies.ai_model_downloads",
        ),
        ("configure.x.media", "settings_ui.proxies.x_post_media"),
    ] {
        let label = t_smol!(harness.services().translate, label_key);
        assert!(
            harness.field(field).is_some(),
            "the configure page should draw the {field} line"
        );
        assert_eq!(harness.field_label(field), label);
    }
    let market_participants = t_smol!(
        harness.services().translate,
        "settings_ui.proxies.market_participants_section"
    );
    let kytrix = t_smol!(
        harness.services().translate,
        "settings_ui.proxies.kytrix_section"
    );
    let title = harness
        .find("SETTINGS_SECTION_TITLE_configure_1")
        .expect("the venue group should be named over its rows");
    assert_eq!(harness.node_text(title), market_participants);
    let title = harness
        .find("SETTINGS_SECTION_TITLE_configure_2")
        .expect("the Kytrix group should be named over its rows");
    assert_eq!(harness.node_text(title), kytrix);
}

/// Choosing "system proxy" as the default line stores it: the line has nothing above it to inherit
/// from, so the choice is written as a line of its own.
#[test]
fn choosing_the_default_line_should_store_it() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("configure");
    harness.settle(1);

    // The default line offers the two built-in lines before the catalog: direct, system.
    choose(
        &mut harness,
        &select_key("configure", "proxy.default-line"),
        1,
    );

    assert_eq!(
        harness.config().proxy_default,
        ProxyRoute::System,
        "the choice should be stored as the default line"
    );
    assert_eq!(harness.writes().len(), 1);
}

/// Choosing a line for a group of Kytrix services binds every target of the group to it.
#[test]
fn choosing_a_group_line_should_bind_every_target() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("configure");
    harness.settle(1);

    choose(
        &mut harness,
        &select_key("configure", "configure.kytrix.cloud"),
        2,
    );

    let applications = &harness.config().proxy_applications;
    assert_eq!(
        applications.len(),
        11,
        "the account and cloud group reaches eleven targets"
    );
    assert!(
        applications
            .values()
            .all(|route| *route == ProxyRoute::System),
        "every target of the group should be bound to the chosen line"
    );
    assert_eq!(harness.writes().len(), 1);
}

/// Choosing a line for a venue stores it as that venue's own line.
#[test]
fn choosing_a_venue_line_should_store_it() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("configure");
    harness.settle(1);

    choose(
        &mut harness,
        &select_key("configure", "configure.binance"),
        2,
    );

    let config = harness.config();
    let venue = config
        .venue(&VenueId::Binance)
        .expect("the default settings name every venue this build speaks");
    assert_eq!(
        venue.proxy(),
        Some(ProxyRoute::System),
        "the choice should be stored as the venue's own line"
    );
    assert_eq!(harness.writes().len(), 1);
}

/// Every line's control offers the entries of the proxy catalog beside the built-in lines.
#[test]
fn every_proxy_line_should_offer_the_catalog_entries() {
    let mut harness = Harness::with_config(configured_settings());
    harness.settle(3);
    harness.select_page("configure");
    harness.settle(1);

    let key = select_key("configure", "proxy.default-line");
    harness.click_node(&format!("{key}:trigger"));
    harness.settle(2);

    // The default line offers the two built-in lines before the catalog: direct, system, hk.
    let row = harness
        .find(&format!("{key}:panel:list:row:2"))
        .expect("the catalog entry should be offered by the line's control");
    assert!(
        harness.has_text(row, "hk"),
        "the catalog entry should be drawn by its name: {}",
        harness.text_of(row)
    );
}

/// A query that names one line reaches the configure page through that line and draws only the
/// rows the query matched.
#[test]
fn a_query_for_a_proxy_line_should_open_the_page_and_draw_only_it() {
    let mut harness = Harness::new();
    harness.settle(3);

    harness.set_query("kytrix cloud");

    assert_eq!(
        harness.page().current_page(),
        "configure",
        "the page that holds the matched line should be the one the query opens"
    );
    let filter = harness.page().filter_for_test();
    assert!(
        filter.admits_field("configure", "configure.kytrix.cloud"),
        "the cloud line's own text should hold both words"
    );
    assert!(
        !filter.admits_field("configure", "configure.binance"),
        "a line the query did not match should not be drawn"
    );
    assert!(
        harness.field("configure.kytrix.cloud").is_some(),
        "the line the query matched should be drawn"
    );
    assert!(
        harness.field("configure.binance").is_none(),
        "a line the query did not match should not be drawn on a page reached by a line"
    );

    harness.set_query("");
    assert!(harness.page().filter_for_test().is_all());
    assert!(
        harness.field("configure.binance").is_some(),
        "clearing the query should bring the whole page back"
    );
}

/// Switching away from a page releases the controls it built: the select it drew stops answering
/// its action, so a page that is gone cannot be pressed.
#[test]
fn switching_pages_should_release_the_controls_of_the_page_left() {
    let mut harness = Harness::new();
    harness.settle(3);

    harness.select_page("data-format");
    harness.settle(1);
    let data_format = trigger_action(&select_key("data-format", "offset"));
    assert!(
        harness.app().has_action_handler(&data_format),
        "the data-format select should be wired while its page is drawn"
    );

    harness.select_page("configure");
    harness.settle(1);
    let configure = trigger_action(&select_key("configure", "proxy.default-line"));
    assert!(
        harness.app().has_action_handler(&configure),
        "the configure select should be wired while its page is drawn"
    );
    assert!(
        !harness.app().has_action_handler(&data_format),
        "leaving the data formatting page should release its select"
    );

    harness.select_page("about");
    harness.settle(1);
    assert!(
        !harness.app().has_action_handler(&configure),
        "leaving the configure page should release the selects it built"
    );
}

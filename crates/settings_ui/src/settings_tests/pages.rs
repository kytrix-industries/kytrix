use app_config::{network::ProxyRoute, time_zone::DataTimeZone};
use i18n::t_smol;
use market_participants_connector::venue::definition::supported_venues;
use market_participants_model::identifier::VenueId;
use ui_core::event::{Action, KeyCode};
use widget::venue::VenueDisplay;

use crate::settings_tests::test_support::{Harness, configured_settings};

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

fn select_key(page: &str, suffix: &str) -> String {
    format!("settings.test.{page}.{suffix}")
}

fn trigger_action(key: &str) -> Action {
    Action::Custom(format!("{key}.trigger").into())
}

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

#[test]
fn choosing_a_time_zone_should_store_it() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("data-format");
    harness.settle(1);

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

#[test]
fn choosing_the_default_line_should_store_it() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.select_page("configure");
    harness.settle(1);

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

#[test]
fn every_proxy_line_should_offer_the_catalog_entries() {
    let mut harness = Harness::with_config(configured_settings());
    harness.settle(3);
    harness.select_page("configure");
    harness.settle(1);

    let key = select_key("configure", "proxy.default-line");
    harness.click_node(&format!("{key}:trigger"));
    harness.settle(2);

    let row = harness
        .find(&format!("{key}:panel:list:row:2"))
        .expect("the catalog entry should be offered by the line's control");
    assert!(
        harness.has_text(row, "hk"),
        "the catalog entry should be drawn by its name: {}",
        harness.text_of(row)
    );
}

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

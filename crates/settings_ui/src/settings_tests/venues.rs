//! The tests of the venue pages: the pages the served venues offer and the values written through
//! them.

use app_config::{
    market_participants::{
        credentials::LongbridgeCredentials,
        origins::{BinanceMarket, LongbridgeRegion},
        settings::VenueSettings,
    },
    settings::Config,
};
use gpui_kit::{SharedString, TestAppContext};
use market_participants_connector::connector::MarketParticipantsConnector;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use super::test_support::*;

/// A venue's page is named by the venue it configures, and the sidebar row is what opens it:
/// every venue this build speaks is one page, so a shared label would make two venues tell apart
/// by nothing the user can read.
#[gpui_kit::test]
fn a_venue_page_should_be_named_by_the_venue_it_configures(cx: &mut TestAppContext) {
    let (_panel, cx) = settings(Config::default(), cx);

    assert_eq!(
        page_title("market_participants.okx", cx),
        SharedString::new_static("OKX"),
        "the venue's page is named by the venue it configures"
    );
    assert_eq!(
        page_title("appearance", cx),
        SharedString::new_static("Appearance"),
        "every other page is named by the message key it declares"
    );
}

/// A group is a heading over the pages under it, and every page under it is drawn: nothing a
/// group declares hides one of them, so the venues the release notes promise are one click away
/// from the first frame.
#[gpui_kit::test]
fn a_group_should_draw_every_page_under_it(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    let opening = labels(&panel, cx);
    assert!(
        opening.contains(&SharedString::new_static("Market participants")),
        "the group should be drawn as a heading: {opening:?}"
    );
    assert!(
        opening.contains(&SharedString::new_static("OKX")),
        "every venue under the group should be drawn on the first frame: {opening:?}"
    );
}

/// A page at the top of the registry is a part of its own rather than another page of the group
/// above it: the divider is what says so, and a page with nothing before it is not divided from
/// anything.
#[gpui_kit::test]
fn a_root_page_should_be_divided_from_the_group_above_it(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    assert!(
        cx.debug_bounds("SETTINGS_SIDEBAR_SEPARATOR").is_some(),
        "About should be divided from the group above it"
    );

    panel.update(cx, |dialog, cx| dialog.set_query("about".into(), cx));
    settle(cx);

    assert!(
        cx.debug_bounds("SETTINGS_SIDEBAR_SEPARATOR").is_none(),
        "a page with nothing before it should not be divided from anything"
    );
    assert_eq!(
        labels(&panel, cx),
        [SharedString::new_static("About")],
        "the query should leave the page the divider belonged to"
    );
}

/// A build that has loaded nothing still offers the rows of its own venues: the venue list is
/// this build's static answer, so a row per venue is there before anything is loaded.
#[gpui_kit::test]
fn an_unconnected_build_should_still_offer_the_declared_settings(cx: &mut TestAppContext) {
    let (dialog, cx) = settings_of_an_unconnected_build(cx);
    let connector = cx.read(|cx| cx.global::<MarketParticipantsConnector>().clone());
    let venues = connector.supported_venues();

    assert!(
        connector.venues().scopes().is_empty(),
        "the settings are opened over an empty loaded-venues view"
    );

    select(&dialog, "configure", cx);
    for venue in &venues {
        let selector =
            Box::leak(format!("SETTINGS_FIELD_configure.{}", venue.as_str()).into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "a venue's line should be drawn whether or not anything is loaded"
        );
    }
    assert!(
        cx.debug_bounds("SETTINGS_FIELD_configure.kytrix.cloud")
            .is_some(),
        "the rows that belong to no market are drawn by a build that has loaded nothing"
    );

    select(&dialog, "market_participants.binance", cx);
    assert!(
        cx.debug_bounds("SETTINGS_FIELD_market_participants.binance.spot.http")
            .is_some(),
        "a venue's origins are its own configuration's answer rather than a load's"
    );
}

/// A write through a venue's own rows lands in that venue's row, and a market keeps the values of
/// the other market.
#[gpui_kit::test]
fn a_write_through_a_venue_row_should_land_in_that_venues_row(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.binance", cx);

    type_into(
        cx,
        "SETTINGS_CONTROL_market_participants.binance.spot.spot_api_key",
        "typed-key",
    );
    panel.update(cx, |panel, cx| {
        let _ = panel.commit_pending(cx);
    });
    settle(cx);

    let stored = stored(cx);
    let VenueSettings::Binance(binance) = stored
        .venue(&VenueId::Binance)
        .expect("the fixture settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.credentials.spot_api_key, "binance-spot-keytyped-key",
        "the value landed in the venue the row belongs to"
    );
    assert_eq!(
        binance.credentials.usd_m_perp_api_key, "binance-perp-key",
        "and the venue's other market kept its own"
    );
}

/// A Binance market's origin row writes the address it chose and carries the other role over:
/// the two addresses are one pair, and a row that wrote one of them must not reset the other.
#[gpui_kit::test]
fn a_binance_origin_row_should_write_the_chosen_address(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.binance", cx);

    click(cx, "SETTINGS_CONTROL_market_participants.binance.spot.http");
    click(cx, "TEST_MENU_ITEM_https://api1.binance.com");

    let stored = stored(cx);
    let Some(VenueSettings::Binance(binance)) = stored.venue(&VenueId::Binance) else {
        panic!("binance is configured as Binance");
    };
    let origins = binance
        .market_origins(BinanceMarket::Spot)
        .expect("the spot market holds its origins");
    assert_eq!(
        origins.http.as_str(),
        "https://api1.binance.com",
        "the row writes the address the user chose"
    );
    assert_eq!(
        origins.websocket.as_str(),
        "wss://stream.binance.com:9443",
        "the other address of the pair is carried over"
    );
}

/// Longbridge is a page of the settings a user can open, not one that exists only in a builder.
///
/// The query narrows the page to its own rows — a row below the fold is one the window never lays
/// out — so every row the page names is asserted rather than the first one that happens to fit.
#[gpui_kit::test]
fn longbridges_page_should_be_a_page_of_the_served_settings(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    assert!(
        !labels(&panel, cx).is_empty(),
        "the sidebar should draw at least one row"
    );

    select(&panel, "market_participants.longbridge", cx);
    panel.update(cx, |panel, cx| {
        panel.set_query("longbridge".into(), cx);
    });
    settle(cx);

    assert_eq!(
        current_page(&panel, cx),
        "market_participants.longbridge",
        "the page the served settings declare is the page that is opened"
    );
    assert_eq!(
        page_title("market_participants.longbridge", cx),
        SharedString::new_static("Longbridge"),
        "the page is named by the venue it configures"
    );

    for field in [
        "SETTINGS_FIELD_market_participants.longbridge.region",
        "SETTINGS_FIELD_market_participants.longbridge.app_key",
        "SETTINGS_FIELD_market_participants.longbridge.app_secret",
        "SETTINGS_FIELD_market_participants.longbridge.access_token",
    ] {
        assert!(
            cx.debug_bounds(field).is_some(),
            "the page should draw the row {field} names"
        );
    }
    for control in [
        "SETTINGS_CONTROL_market_participants.longbridge.app_key",
        "SETTINGS_CONTROL_market_participants.longbridge.app_secret",
        "SETTINGS_CONTROL_market_participants.longbridge.access_token",
    ] {
        assert!(
            cx.debug_bounds(control).is_some(),
            "the page should draw the control behind {control}"
        );
    }
}

/// What a user configures on Longbridge's page is what the settings hold: the region moves the
/// site the venue is reached on, and the three values land in the venue's own credential set, the
/// region at once and the text fields when they are left.
#[gpui_kit::test]
fn longbridges_page_should_write_the_region_and_the_credentials(cx: &mut TestAppContext) {
    let longbridge = VenueId::Longbridge;
    let (panel, cx) = settings(Config::default(), cx);

    select(&panel, "market_participants.longbridge", cx);
    click(cx, "SETTINGS_CONTROL_market_participants.longbridge.region");
    click(cx, "TEST_MENU_ITEM_china");

    for (selector, value) in [
        (
            "SETTINGS_CONTROL_market_participants.longbridge.app_key",
            "typed-app-key",
        ),
        (
            "SETTINGS_CONTROL_market_participants.longbridge.app_secret",
            "typed-app-secret",
        ),
        (
            "SETTINGS_CONTROL_market_participants.longbridge.access_token",
            "typed-access-token",
        ),
    ] {
        type_into(cx, selector, value);
    }
    panel.update(cx, |panel, cx| {
        let _ = panel.commit_pending(cx);
    });
    settle(cx);

    let written = stored(cx);
    let Some(VenueSettings::Longbridge(settings)) = written.venue(&longbridge) else {
        panic!("longbridge is configured as Longbridge");
    };
    assert_eq!(
        settings.credentials,
        LongbridgeCredentials {
            app_key: SmolStr::new_static("typed-app-key"),
            app_secret: SmolStr::new_static("typed-app-secret"),
            access_token: SmolStr::new_static("typed-access-token"),
        },
        "the three values landed in the venue's own credential set"
    );
    assert_eq!(
        settings.region,
        LongbridgeRegion::China,
        "the region row wrote the region the user chose"
    );
    assert_eq!(
        settings.addresses().http.as_str(),
        "https://openapi.longbridge.cn/",
        "and the venue is reached on the site that region publishes"
    );
}

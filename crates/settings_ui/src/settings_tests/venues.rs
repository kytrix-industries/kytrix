//! The tests of the venue pages: the pages the served venues offer and the values written through
//! them.

use app_config::{
    market_participants::{
        credentials::{BinanceMarketCredentials, LongbridgeCredentials},
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
use crate::pages::venues::credentials::{CONFIRM_SELECTOR, CredentialsKind};

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

/// A write through one market's credential dialog lands in that market's set, and the venue's other
/// market keeps its own: the two markets are configured apart because they sign apart.
#[gpui_kit::test]
fn a_binance_markets_credentials_should_be_written_apart(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.binance", cx);
    let kind = CredentialsKind::BinanceSpot;

    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "api_key"), "typed-key");
    click(cx, CONFIRM_SELECTOR);

    let stored = stored(cx);
    let VenueSettings::Binance(binance) = stored
        .venue(&VenueId::Binance)
        .expect("the fixture settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.spot_credentials,
        Some(
            app_config::market_participants::credentials::BinanceMarketCredentials {
                api_key: SmolStr::new_static("binance-spot-keytyped-key"),
                private_key: SmolStr::new_static("binance-spot-private"),
                passphrase: SmolStr::new_static("binance-spot-passphrase"),
            }
        ),
        "the value landed in the market the dialog belongs to"
    );
    assert_eq!(
        binance
            .usd_m_perp_credentials
            .as_ref()
            .map(|credentials| credentials.api_key.as_str()),
        Some("binance-perp-key"),
        "and the venue's other market kept its own set"
    );
    assert_eq!(
        binance
            .market_credentials(BinanceMarket::Spot)
            .map(|credentials| credentials.api_key.as_str()),
        Some("binance-spot-keytyped-key"),
        "the stored set is the one the spot market signs with"
    );
}

/// A write through the futures market's dialog lands in that market's set, and a clear through the
/// same market's row takes only that set: the spot set is exactly as it was either way, because
/// the two markets are configured apart, whichever of the two rows the user acts on.
#[gpui_kit::test]
fn a_binance_usd_m_perp_dialog_should_leave_the_spot_set_alone(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.binance", cx);
    let kind = CredentialsKind::BinanceUsdMPerp;

    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "api_key"), "typed-key");
    click(cx, CONFIRM_SELECTOR);

    let written = stored(cx);
    let VenueSettings::Binance(binance) = written
        .venue(&VenueId::Binance)
        .expect("the fixture settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.spot_credentials,
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("binance-spot-key"),
            private_key: SmolStr::new_static("binance-spot-private"),
            passphrase: SmolStr::new_static("binance-spot-passphrase"),
        }),
        "a write through the futures dialog must not reach the spot set"
    );
    assert_eq!(
        binance
            .usd_m_perp_credentials
            .as_ref()
            .map(|credentials| credentials.api_key.as_str()),
        Some("binance-perp-keytyped-key"),
        "the value landed in the market the dialog belongs to"
    );

    click(cx, &credentials_clear(kind));
    click(cx, "BASE_UI_ALERT_OK");

    let cleared = stored(cx);
    let VenueSettings::Binance(binance) = cleared
        .venue(&VenueId::Binance)
        .expect("the fixture settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.usd_m_perp_credentials, None,
        "confirming the alert should clear the futures market's set"
    );
    assert_eq!(
        binance.spot_credentials,
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("binance-spot-key"),
            private_key: SmolStr::new_static("binance-spot-private"),
            passphrase: SmolStr::new_static("binance-spot-passphrase"),
        }),
        "a clear through the futures row must not reach the spot set"
    );
}

/// Each Binance market's credential row is one row of that market's own card, drawn under the
/// market's two addresses: the credentials are one setting of the market rather than a card of
/// their own, and the other market's rows are not in the card the first market's are in.
#[gpui_kit::test]
fn each_binance_market_should_draw_its_credentials_under_its_own_addresses(
    cx: &mut TestAppContext,
) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.binance", cx);

    for (kind, card_id, http_id, websocket_id) in [
        (
            CredentialsKind::BinanceSpot,
            "SETTINGS_CARD_market_participants.binance_0",
            "SETTINGS_FIELD_market_participants.binance.spot.http",
            "SETTINGS_FIELD_market_participants.binance.spot.websocket",
        ),
        (
            CredentialsKind::BinanceUsdMPerp,
            "SETTINGS_CARD_market_participants.binance_1",
            "SETTINGS_FIELD_market_participants.binance.usd-m-perp.http",
            "SETTINGS_FIELD_market_participants.binance.usd-m-perp.websocket",
        ),
    ] {
        let card = element(cx, card_id);
        let http = element(cx, http_id);
        let websocket = element(cx, websocket_id);
        let credentials = element(cx, &credentials_row(kind));

        assert!(
            card.contains(&credentials.center()),
            "{kind:?}'s credential row belongs to the card its market's rows are drawn in: \
             {credentials:?} in {card:?}"
        );
        assert!(
            card.contains(&http.center()) && card.contains(&websocket.center()),
            "{kind:?}'s addresses belong to the same card: {http:?} and {websocket:?} in {card:?}"
        );
        assert!(
            http.top() < websocket.top() && websocket.bottom() <= credentials.top(),
            "{kind:?}'s credential row belongs under both addresses: {http:?}, {websocket:?}, \
             {credentials:?}"
        );
    }

    for selector in [
        "SETTINGS_CARD_market_participants.binance_2",
        "SETTINGS_CARD_market_participants.binance.spot.credentials",
        "SETTINGS_CARD_market_participants.binance.usd-m-perp.credentials",
    ] {
        assert!(
            bounds(cx, selector).is_none(),
            "the credentials are a row of a market's card rather than a card of their own: \
             {selector} should not be drawn"
        );
    }
}

/// A credential value is edited in the dialog rather than drawn as a row of its own: the pages of
/// the venues this build serves draw the settings that are not secrets, and no page draws an old
/// per-value row beside the set that holds it.
#[gpui_kit::test]
fn a_venues_credentials_should_be_edited_in_a_dialog_rather_than_in_rows(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    select(&panel, "market_participants.binance", cx);
    for selector in [
        "SETTINGS_FIELD_market_participants.binance.spot.spot_api_key",
        "SETTINGS_FIELD_market_participants.binance.spot.api_key",
        "SETTINGS_FIELD_market_participants.binance.spot.private_key",
        "SETTINGS_FIELD_market_participants.binance.spot.passphrase",
        "SETTINGS_FIELD_market_participants.binance.usd-m-perp.usd_m_perp_api_key",
        "SETTINGS_FIELD_market_participants.binance.usd-m-perp.api_key",
        "SETTINGS_FIELD_market_participants.binance.usd-m-perp.private_key",
        "SETTINGS_FIELD_market_participants.binance.usd-m-perp.passphrase",
    ] {
        assert!(
            bounds(cx, selector).is_none(),
            "a Binance credential value should not be a row of its own: {selector} was drawn"
        );
    }
    assert!(
        bounds(cx, &credentials_row(CredentialsKind::BinanceSpot)).is_some()
            && bounds(cx, &credentials_row(CredentialsKind::BinanceUsdMPerp)).is_some(),
        "both markets' credential sets should be drawn as the one row each they are"
    );

    select(&panel, "market_participants.okx", cx);
    for selector in [
        "SETTINGS_FIELD_market_participants.okx.api_key",
        "SETTINGS_FIELD_market_participants.okx.secret_key",
        "SETTINGS_FIELD_market_participants.okx.passphrase",
    ] {
        assert!(
            bounds(cx, selector).is_none(),
            "an OKX credential value should not be a row of its own: {selector} was drawn"
        );
    }

    select(&panel, "market_participants.hyperliquid", cx);
    for selector in [
        "SETTINGS_FIELD_market_participants.hyperliquid.main_account_address",
        "SETTINGS_FIELD_market_participants.hyperliquid.api_agent_private_key",
    ] {
        assert!(
            bounds(cx, selector).is_none(),
            "a Hyperliquid credential value should not be a row of its own: {selector} was drawn"
        );
    }
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
/// The credential set is one row rather than one row per value: the values are edited in a dialog,
/// and what the page says about them is whether they are configured.
#[gpui_kit::test]
fn longbridges_page_should_be_a_page_of_the_served_settings(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    let kind = CredentialsKind::Longbridge;

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

    let region = element(cx, "SETTINGS_FIELD_market_participants.longbridge.region");
    let credentials = element(cx, &credentials_row(kind));
    let card = element(cx, "SETTINGS_CARD_market_participants.longbridge_0");
    assert!(
        card.contains(&region.center()) && card.contains(&credentials.center()),
        "the region and the credential set are two rows of one card: {region:?} and \
         {credentials:?} in {card:?}"
    );
    assert!(
        bounds(cx, &kind.edit_selector()).is_some(),
        "a configured venue's credential row offers the dialog that edits it"
    );
    assert!(
        bounds(cx, &credentials_clear(kind)).is_some(),
        "and the clear button, because the fixture stores a set"
    );
    for field in ["app_key", "app_secret", "access_token"] {
        assert!(
            bounds(
                cx,
                &format!("SETTINGS_FIELD_market_participants.longbridge.{field}")
            )
            .is_none(),
            "the value {field} is edited in the dialog rather than drawn as a row of its own"
        );
    }
}

/// What a user configures on Longbridge's page is what the settings hold: the region moves the
/// site the venue is reached on, and the credential dialog writes the venue's own set.
#[gpui_kit::test]
fn longbridges_page_should_write_the_region_and_the_credentials(cx: &mut TestAppContext) {
    let longbridge = VenueId::Longbridge;
    let kind = CredentialsKind::Longbridge;
    let (panel, cx) = settings(Config::default(), cx);

    select(&panel, "market_participants.longbridge", cx);
    click(cx, "SETTINGS_CONTROL_market_participants.longbridge.region");
    click(cx, "TEST_MENU_ITEM_china");

    click(cx, &kind.edit_selector());
    for (field, value) in [
        ("app_key", "typed-app-key"),
        ("app_secret", "typed-app-secret"),
        ("access_token", "typed-access-token"),
    ] {
        type_into(cx, &credentials_field(kind, field), value);
    }
    click(cx, CONFIRM_SELECTOR);

    let written = stored(cx);
    let Some(VenueSettings::Longbridge(settings)) = written.venue(&longbridge) else {
        panic!("longbridge is configured as Longbridge");
    };
    assert_eq!(
        settings.credentials,
        Some(LongbridgeCredentials {
            app_key: SmolStr::new_static("typed-app-key"),
            app_secret: SmolStr::new_static("typed-app-secret"),
            access_token: SmolStr::new_static("typed-access-token"),
        }),
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

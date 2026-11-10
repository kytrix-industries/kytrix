//! The tests of the venue pages: the catalog's venues and the values written through them.

use app_config::{
    market_participants::{
        credentials::LongbridgeCredentials, origins::LongbridgeRegion, settings::VenueSettings,
    },
    settings::Config,
};
use gpui_kit::{SharedString, TestAppContext};
use market_participants_connector::connector::MarketParticipantsConnector;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use super::test_support::*;
use crate::{access::FieldValue, catalog::catalog};

/// A venue's page is named by the venue it configures, and the sidebar row is what opens
/// it: every venue this build speaks is one page, so a shared label would make two venues
/// tell apart by nothing the user can read.
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

/// A group is a heading over the pages under it, and every page under it is drawn: nothing
/// a group declares hides one of them, so the venues the release notes promise are one
/// click away from the first frame.
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

/// A page at the top of the catalog is a part of its own rather than another page of the
/// group above it: the divider is what says so, and a page with nothing before it is not
/// divided from anything.
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

/// A build that has loaded nothing still offers the rows of its own venues: the venue list
/// is this build's static answer, so a row per venue is there before anything is
/// loaded, while a scope line is what the loaded venues answer and appears only once its
/// venue is loaded.
///
/// What is asserted here is the dialog the entry's own read of the loaded venues built —
/// not a catalog assembled beside it — so a page source that went back to a declaration
/// view, or one that read a scope list before its venue was loaded, would be caught here.
#[gpui_kit::test]
fn an_unconnected_build_should_still_offer_the_declared_settings(cx: &mut TestAppContext) {
    let (dialog, cx) = settings_of_an_unconnected_build(cx);
    let connector = cx.read(|cx| cx.global::<MarketParticipantsConnector>().clone());
    let venues = connector.supported_venues();

    assert!(
        connector.venues().scopes().is_empty(),
        "the settings are opened over an empty loaded-venues view"
    );

    let applications = dialog.read_with(cx, |dialog, _| {
        dialog
            .catalog
            .page("proxy-applications")
            .expect("the applications page is built")
            .clone()
    });
    let rows = applications
        .sections
        .iter()
        .filter(|section| {
            section.title.as_ref().and_then(crate::catalog::Text::key)
                == Some("settings_ui.proxies.venue_section")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        venues.len(),
        "one row per venue this build speaks, whether or not anything is loaded"
    );
    for (row, venue) in rows.iter().zip(venues.iter()) {
        assert_eq!(
            row.fields.len(),
            1,
            "a venue's own line is the whole of its row"
        );
        assert_eq!(
            row.fields[0].id,
            format!("proxy-applications.{}", venue.as_str()),
            "the row stands for the venue it is drawn under"
        );
    }

    // The page is drawn rather than skipped: the proxy lines that are not a market's are
    // there for a build that has loaded nothing.
    type_into(cx, "SETTINGS_SEARCH", "applications");
    assert!(
        cx.debug_bounds("SETTINGS_FIELD_proxy-applications.kytrix.cloud")
            .is_some(),
        "the rows that belong to no market are drawn by a build that has loaded nothing"
    );

    // A venue's origins are its own configuration's answer rather than a load's, so they are
    // there for a build that has loaded nothing — and a load does not add or move one.
    let binance = VenueId::Binance;
    let page = catalog(&crate::schema::tests::venues())
        .page("market_participants.binance")
        .expect("the venue's page is built")
        .clone();
    let scope = page
        .sections
        .iter()
        .flat_map(|section| &section.fields)
        .find(|field| field.label.key().is_none())
        .expect("a venue that has loaded nothing still offers the origin of a scope")
        .clone();
    let before = page
        .sections
        .iter()
        .flat_map(|section| &section.fields)
        .map(|field| field.id.clone())
        .collect::<Vec<_>>();

    cx.update(|_, cx| {
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(
                &market_participants_connector::test_support::definitions(&venues),
                &[],
                &[],
            )
            .expect("the declared scopes load");
    });
    let loaded = catalog(&crate::schema::tests::venues());
    let after = loaded
        .page("market_participants.binance")
        .expect("the venue's page is built")
        .sections
        .iter()
        .flat_map(|section| &section.fields)
        .map(|field| field.id.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        before, after,
        "loading a venue does not add or move an origin row: the rows are the stored scopes"
    );
    assert!(
        scope
            .id
            .as_ref()
            .starts_with(&format!("market_participants.{}.", binance.as_str())),
        "the origin row stands for a scope of its venue: {}",
        scope.id
    );
}

/// A write that moves a venue's credential shape moves the page with it: the fields the user
/// is looking at are the ones the shape the venue holds now has, so what they type into the
/// page as it is drawn lands in that shape rather than nowhere.
///
/// A write through a control of a venue's page lands in that venue's own row, and a venue's
/// market holds its own values: what a control offers is the venue's own API rather than
/// anything a value moves.
///
/// The write goes through the field's own access, which is what the control of that field
/// holds — the same conversion the dialog's `write_field` is handed — so this is about where a
/// page's write lands, not about where a click lands.
#[gpui_kit::test]
fn a_write_through_a_venue_row_should_land_in_that_venues_row(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.binance", cx);

    let spot = "SETTINGS_CONTROL_market_participants.binance.spot.spot_api_key";
    assert!(
        cx.debug_bounds(spot).is_some(),
        "the page opens on the market the venue publishes"
    );

    let access = field_access(
        "market_participants.binance",
        "market_participants.binance.spot.spot_api_key",
    );
    panel.update(cx, |panel, cx| {
        panel.write_field(access, FieldValue::new_static("typed-key"), cx);
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
        binance.credentials.spot_api_key, "typed-key",
        "the value landed in the venue the row belongs to"
    );
    assert_eq!(
        binance.credentials.usd_m_perp_api_key, "binance-perp-key",
        "and the venue's other market kept its own"
    );
}

/// Longbridge is a page of the settings a user can open, not one that exists only in the
/// catalog's own builder.
///
/// The dialog builds its venue pages from the venue list this build *serves*, so a venue a
/// previous build declared settings for without serving it is a page nothing can reach: this
/// opens that page through the served list and reads the boxes its rows are drawn in. The
/// query narrows the page to its own rows — a page is drawn inside a scroll viewport, and a
/// row below the fold is one the window never lays out — so every row the design names is
/// asserted rather than the first one that happens to fit.
///
/// A row's *control* is asserted for the rows a text field draws. The region row's control is
/// a `Select`, which registers no debug bounds of its own — the same is true of every select
/// in the settings, the appearance page's and OKX's region among them — so what is asserted
/// for that row is that the row itself is drawn; which choice it offers is the catalogue's,
/// and the page's own round trip below is what writes through it.
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
            "the page should draw the field behind {control}"
        );
    }
}

/// What a user configures on Longbridge's page is what the settings hold: the region moves
/// the site the venue is reached on, and the three values land in the venue's own credential
/// set, the region at once and the text fields when they are left.
///
/// The write goes through the page's own rows, over the manager the settings session was
/// opened from — which is what makes this a round trip through the settings rather than
/// through the configuration type: the values are read back from the manager every other part
/// of the application reads.
#[gpui_kit::test]
fn longbridges_page_should_write_the_region_and_the_credentials(cx: &mut TestAppContext) {
    let longbridge = VenueId::Longbridge;
    let (panel, cx) = settings(Config::default(), cx);

    select(&panel, "market_participants.longbridge", cx);

    write_field(
        &panel,
        "market_participants.longbridge",
        "market_participants.longbridge.region",
        "china",
        cx,
    );

    // The three values are typed into the controls the page draws, and leaving the page is
    // what reads them: a text row writes when the caret leaves it.
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

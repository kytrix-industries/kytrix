//! The tests of the venue pages: the pages the served venues offer and the values written through
//! them.
//!
//! The helpers below are the three interactions a venue page is driven with — opening the page from
//! the sidebar, choosing an option of one of its selectors, and typing into the credential dialog —
//! and the geometry reads the layout assertions use. They live here because the venue pages are the
//! only pages that draw them, and [`crate::settings_tests::credentials`] uses the same helpers.

use app_config::{
    market_participants::{
        credentials::BinanceMarketCredentials,
        origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin, LongbridgeRegion},
        settings::VenueSettings,
    },
    settings::{Config, Language},
};
use i18n::language::codes::LanguageCode;
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    math::{Point, Rect},
    node::id::NodeId,
};

use crate::{
    pages::venues::credentials::{
        self, CANCEL_SELECTOR, CONFIRM_SELECTOR, CredentialsKind, DIALOG_SELECTOR,
    },
    settings_tests::test_support::{Harness, configured_settings, translate_in},
};

/// The page key every test harness mounts the settings under.
pub(crate) const HARNESS_KEY: &str = "settings.test";

/// The configured fixture with the interface drawn in English.
pub(crate) fn english_settings() -> Config {
    let mut config = configured_settings();
    config.language = Language::En;
    config
}

/// Opens one venue's page from the sidebar.
pub(crate) fn open_page(harness: &mut Harness, page: &str) {
    harness.settle(2);
    harness.select_page(page);
    harness.settle(1);
}

/// The application-level key the credential controls of one set are named by.
pub(crate) fn credentials_key(kind: CredentialsKind) -> SmolStr {
    format_smolstr!("{HARNESS_KEY}.{}", kind.row_id())
}

/// The key the dialog that edits one set is named by.
pub(crate) fn dialog_key(kind: CredentialsKind) -> SmolStr {
    format_smolstr!("{}.dialog", credentials_key(kind))
}

/// One value of one set, as the set declares it.
pub(crate) fn field_of(kind: CredentialsKind, id: &str) -> &'static credentials::CredentialField {
    kind.fields()
        .iter()
        .find(|field| field.id == id)
        .unwrap_or_else(|| panic!("{kind:?} declares no value {id}"))
}

/// The id one value of one set is keyed under.
pub(crate) fn field_id(kind: CredentialsKind, id: &str) -> SmolStr {
    kind.field_id(field_of(kind, id))
}

/// Runs the frames a dialog's fade needs, the way the platform's clock advances it.
///
/// A dialog that is fading out still holds the layer its modal scope is over: until the fade
/// finishes, a click on the row underneath it would go to the scrim rather than the row.
pub(crate) fn settle_animation(harness: &mut Harness) {
    for _ in 0..20 {
        harness.advance(16);
    }
}

/// Opens the dialog that edits one set from its row.
pub(crate) fn open_credentials_dialog(harness: &mut Harness, kind: CredentialsKind) {
    harness.click_node(kind.edit_selector().as_ref());
    harness.settle(3);
    assert!(
        harness.find(DIALOG_SELECTOR).is_some(),
        "the row's edit button should open the dialog of {kind:?}"
    );
}

/// Clicks the dialog's confirm button.
pub(crate) fn confirm_dialog(harness: &mut Harness) {
    harness.click_node(CONFIRM_SELECTOR);
    settle_animation(harness);
}

/// Clicks the dialog's cancel button.
pub(crate) fn cancel_dialog(harness: &mut Harness) {
    harness.click_node(CANCEL_SELECTOR);
    settle_animation(harness);
}

/// Types `text` into one value of the open dialog.
pub(crate) fn type_into(harness: &mut Harness, kind: CredentialsKind, id: &str, text: &str) {
    let name = field_id(kind, id);
    harness.click_node(&format!("{name}.root"));
    harness.settle(1);
    harness.type_text(text);
    harness.settle(1);
}

/// The text one field's line of the open dialog reports, or nothing while it reports nothing.
pub(crate) fn field_error(harness: &Harness, kind: CredentialsKind, id: &str) -> String {
    let selector = kind.error_selector(field_of(kind, id));
    let node = harness
        .find(selector.as_ref())
        .unwrap_or_else(|| panic!("the dialog draws the report line of {id}"));
    harness.text_of(node)
}

/// Chooses one option of a selector a venue page draws.
pub(crate) fn choose_option(harness: &mut Harness, field: &str, index: usize) {
    harness.click_node(&format!("SETTINGS_CONTROL_{field}"));
    harness.settle(2);
    let select_key = format_smolstr!("{HARNESS_KEY}.{field}");
    harness.click_node(&format!("{select_key}:panel:list:row:{index}"));
    harness.settle(2);
}

/// The node whose debug id ends with `suffix`.
///
/// A kit component some of these tests click is named by a key the component mints rather than by
/// an id the page chooses (the alert's two buttons are the case): the whole tree is walked once and
/// the last segment is what tells the one button from the other.
pub(crate) fn find_ending(harness: &Harness, suffix: &str) -> Option<NodeId> {
    let ids = harness.debug_ids();
    let nodes = harness.find_all("");
    assert_eq!(
        ids.len(),
        nodes.len(),
        "the debug ids and the nodes they belong to are read from one tree"
    );
    ids.iter()
        .zip(nodes)
        .find(|(id, _)| id.ends_with(suffix))
        .map(|(_, node)| node)
}

/// Clicks one button of the alert that is open (`"ok"` or `"cancel"`).
pub(crate) fn click_alert(harness: &mut Harness, button: &str) {
    let suffix = format_smolstr!(".{button}:interactive");
    let node = find_ending(harness, suffix.as_str())
        .unwrap_or_else(|| panic!("no alert draws the button {button}"));
    let bounds = harness
        .bounds(node)
        .unwrap_or_else(|| panic!("the alert's {button} button has not been laid out"));
    harness.click(Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    ));
    settle_animation(harness);
}

/// Whether an alert is open (its OK button is what only an alert draws).
pub(crate) fn alert_is_open(harness: &Harness) -> bool {
    find_ending(harness, ".ok:interactive").is_some()
}

/// The layout box of the first node whose debug id starts with `prefix`.
pub(crate) fn bounds(harness: &Harness, prefix: &str) -> Rect {
    let node = harness
        .find(prefix)
        .unwrap_or_else(|| panic!("no node whose id starts with {prefix}"));
    harness
        .bounds(node)
        .unwrap_or_else(|| panic!("{prefix} has not been laid out"))
}

/// The centre of a layout box.
pub(crate) fn center(bounds: Rect) -> Point {
    Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    )
}

/// The `(top, right, bottom, left)` edges of a layout box.
pub(crate) fn edges(bounds: Rect) -> (f32, f32, f32, f32) {
    (
        bounds.origin.y,
        bounds.origin.x + bounds.size.width,
        bounds.origin.y + bounds.size.height,
        bounds.origin.x,
    )
}

/// A page title is the venue's own name, and every other page is named by its message key.
#[test]
fn a_venue_page_should_be_named_by_the_venue_it_configures() {
    let mut harness = Harness::new();
    harness.settle(2);
    let labels = harness.labels();

    assert!(
        labels.contains(&SmolStr::new_static("OKX")),
        "the venue's page is named by the venue it configures: {labels:?}"
    );
    assert!(
        labels.contains(&SmolStr::new_static("Appearance")),
        "every other page is named by the message key it declares: {labels:?}"
    );
}

/// A group is a heading over the pages under it, and every page under it is drawn.
#[test]
fn a_group_should_draw_every_page_under_it() {
    let mut harness = Harness::with_config(english_settings());
    harness.settle(2);
    let labels = harness.labels();

    assert!(
        labels.contains(&SmolStr::new_static("Market participants")),
        "the group should be drawn as a heading: {labels:?}"
    );
    for venue in ["Binance", "OKX", "Hyperliquid", "Longbridge", "Polymarket"] {
        assert!(
            labels.contains(&SmolStr::new_static(venue)),
            "every venue under the group should be drawn on the first frame: {venue} in {labels:?}"
        );
    }
}

/// A page at the top of the registry is a part of its own rather than another page of the group
/// above it: a query that names it leaves it alone in the sidebar.
#[test]
fn a_root_page_should_be_drawn_without_the_group_above_it() {
    let mut harness = Harness::with_config(english_settings());
    harness.settle(2);
    harness.set_query("about");

    assert_eq!(
        harness.page().current_page().as_str(),
        "about",
        "the query should open the page it names"
    );
    assert_eq!(
        harness.labels(),
        [SmolStr::new_static("About")],
        "a root page is drawn without the groups above it"
    );
}

/// A build that has loaded nothing still offers the rows of its own venues.
#[test]
fn a_venue_page_should_offer_its_settings_before_anything_is_loaded() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.binance");

    assert!(
        harness
            .field("market_participants.binance.spot.http")
            .is_some(),
        "a venue's origins are its own configuration's answer rather than a load's"
    );
}

/// A write through one market's credential dialog lands in that market's set, and the venue's other
/// market keeps its own.
#[test]
fn a_binance_markets_credentials_should_be_written_apart() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.binance");
    let kind = CredentialsKind::BinanceSpot;

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "typed-key");
    confirm_dialog(&mut harness);

    let stored = harness.config();
    let VenueSettings::Binance(binance) = stored
        .venue(&VenueId::Binance)
        .expect("the fixture settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.spot_credentials,
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("binance-spot-keytyped-key"),
            private_key: SmolStr::new_static("binance-spot-private"),
            passphrase: SmolStr::new_static("binance-spot-passphrase"),
        }),
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
}

/// A write through the futures market's dialog lands in that market's set, and a clear through the
/// same market's row takes only that set: the spot set is exactly as it was either way.
#[test]
fn a_binance_usd_m_perp_dialog_should_leave_the_spot_set_alone() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.binance");
    let kind = CredentialsKind::BinanceUsdMPerp;

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "typed-key");
    confirm_dialog(&mut harness);

    let stored = harness.config();
    let VenueSettings::Binance(binance) = stored
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

    harness.click_node(kind.clear_selector().as_ref());
    harness.settle(3);
    assert!(alert_is_open(&harness), "clearing is put to the user first");
    click_alert(&mut harness, "ok");

    let cleared = harness.config();
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
    assert!(
        binance.spot_credentials.is_some(),
        "a clear through the futures row must not reach the spot set"
    );
}

/// Each Binance market's credential row is one row of that market's own card, drawn under the
/// market's two addresses.
#[test]
fn each_binance_market_should_draw_its_credentials_under_its_own_addresses() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.binance");

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
        let card = bounds(&harness, card_id);
        let http = bounds(&harness, http_id);
        let websocket = bounds(&harness, websocket_id);
        let credentials = bounds(&harness, &format!("SETTINGS_FIELD_{}", kind.row_id()));

        assert!(
            card.contains(center(credentials)),
            "{kind:?}'s credential row belongs to the card its market's rows are drawn in: \
             {credentials:?} in {card:?}"
        );
        assert!(
            card.contains(center(http)) && card.contains(center(websocket)),
            "{kind:?}'s addresses belong to the same card: {http:?} and {websocket:?} in {card:?}"
        );
        let (http_top, _, _, _) = edges(http);
        let (websocket_top, _, websocket_bottom, _) = edges(websocket);
        let (credentials_top, _, _, _) = edges(credentials);
        assert!(
            http_top < websocket_top && websocket_bottom <= credentials_top,
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
            harness.find(selector).is_none(),
            "the credentials are a row of a market's card rather than a card of their own: \
             {selector} should not be drawn"
        );
    }
}

/// A credential value is edited in the dialog rather than drawn as a row of its own.
#[test]
fn a_venues_credentials_should_be_edited_in_a_dialog_rather_than_in_rows() {
    let mut harness = Harness::with_config(english_settings());

    open_page(&mut harness, "market_participants.binance");
    for selector in [
        "market_participants.binance.spot.spot_api_key",
        "market_participants.binance.spot.api_key",
        "market_participants.binance.spot.private_key",
        "market_participants.binance.spot.passphrase",
        "market_participants.binance.usd-m-perp.usd_m_perp_api_key",
        "market_participants.binance.usd-m-perp.api_key",
        "market_participants.binance.usd-m-perp.private_key",
        "market_participants.binance.usd-m-perp.passphrase",
    ] {
        assert!(
            harness.field(selector).is_none(),
            "a Binance credential value should not be a row of its own: {selector} was drawn"
        );
    }
    assert!(
        harness
            .field(CredentialsKind::BinanceSpot.row_id())
            .is_some()
            && harness
                .field(CredentialsKind::BinanceUsdMPerp.row_id())
                .is_some(),
        "both markets' credential sets should be drawn as the one row each they are"
    );

    open_page(&mut harness, "market_participants.okx");
    for selector in [
        "market_participants.okx.api_key",
        "market_participants.okx.secret_key",
        "market_participants.okx.passphrase",
    ] {
        assert!(
            harness.field(selector).is_none(),
            "an OKX credential value should not be a row of its own: {selector} was drawn"
        );
    }

    open_page(&mut harness, "market_participants.hyperliquid");
    for selector in [
        "market_participants.hyperliquid.main_account_address",
        "market_participants.hyperliquid.api_agent_private_key",
    ] {
        assert!(
            harness.field(selector).is_none(),
            "a Hyperliquid credential value should not be a row of its own: {selector} was drawn"
        );
    }
}

/// A Binance market's origin row writes the address it chose and carries the other role over.
#[test]
fn a_binance_origin_row_should_write_the_chosen_address() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.binance");
    let chosen = "https://api1.binance.com";
    let index = BinanceHttpOrigin::for_market(BinanceMarket::Spot)
        .iter()
        .position(|origin| origin.as_str() == chosen)
        .expect("the spot market publishes the address the test chooses");

    choose_option(&mut harness, "market_participants.binance.spot.http", index);

    let stored = harness.config();
    let Some(VenueSettings::Binance(binance)) = stored.venue(&VenueId::Binance) else {
        panic!("binance is configured as Binance");
    };
    let origins = binance
        .market_origins(BinanceMarket::Spot)
        .expect("the spot market holds its origins");
    assert_eq!(
        origins.http.as_str(),
        chosen,
        "the row writes the address the user chose"
    );
    assert_eq!(
        origins.websocket.as_str(),
        "wss://stream.binance.com:9443",
        "the other address of the pair is carried over"
    );
}

/// A selector shows the value the settings hold, not the value it was built with: a change made
/// from elsewhere (another view, the composition root) is what the next frame draws.
#[test]
fn an_origin_row_should_show_the_address_the_settings_hold() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.binance");
    let trigger =
        format_smolstr!("{HARNESS_KEY}.market_participants.binance.spot.http:trigger:interactive");
    let trigger = harness
        .find(trigger.as_str())
        .expect("the origin row draws its selector");

    let built_with = BinanceMarket::Spot.http().as_str();
    assert!(
        harness.has_text(trigger, built_with),
        "the selector starts on the address the settings hold: {built_with}"
    );

    let chosen = BinanceHttpOrigin::for_market(BinanceMarket::Spot)[1];
    let websocket = BinanceWebsocketOrigin::for_market(BinanceMarket::Spot)[0];
    harness.update_config(|config| {
        config.set_binance_market_origins(
            &VenueId::Binance,
            BinanceMarket::Spot,
            &app_config::market_participants::settings::BinanceOrigins {
                http: chosen,
                websocket,
            },
        );
    });
    harness.settle(2);

    assert!(
        harness.has_text(trigger, chosen.as_str()),
        "the selector follows the settings: it should draw {}",
        chosen.as_str()
    );
    assert!(
        !harness.has_text(trigger, built_with),
        "and it should no longer draw the address it was built with"
    );
}

/// A search that names one row draws that row alone: the filter the shell answers the query with is
/// what decides whether a venue page draws a setting.
#[test]
fn a_search_that_names_one_row_should_draw_only_that_row() {
    let mut harness = Harness::with_config(english_settings());
    harness.settle(2);
    harness.set_query("websocket");
    harness.select_page("market_participants.binance");

    assert!(
        harness
            .field("market_participants.binance.spot.websocket")
            .is_some(),
        "the row the query names should be drawn"
    );
    assert!(
        harness
            .field("market_participants.binance.spot.http")
            .is_none(),
        "a row the query does not name should not be drawn"
    );
}

/// Longbridge is a page of the settings a user can open, not one that exists only in a builder.
#[test]
fn longbridges_page_should_be_a_page_of_the_served_settings() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.longbridge");
    let kind = CredentialsKind::Longbridge;

    assert_eq!(
        harness.page().current_page().as_str(),
        "market_participants.longbridge",
        "the page the served settings declare is the page that is opened"
    );

    let region = bounds(
        &harness,
        "SETTINGS_FIELD_market_participants.longbridge.region",
    );
    let credentials = bounds(
        &harness,
        "SETTINGS_FIELD_market_participants.longbridge.credentials",
    );
    let card = bounds(&harness, "SETTINGS_CARD_market_participants.longbridge_0");
    assert!(
        card.contains(center(region)) && card.contains(center(credentials)),
        "the region and the credential set are two rows of one card: {region:?} and \
         {credentials:?} in {card:?}"
    );
    assert!(
        harness.find(kind.edit_selector().as_ref()).is_some(),
        "a configured venue's credential row offers the dialog that edits it"
    );
    assert!(
        harness.find(kind.clear_selector().as_ref()).is_some(),
        "and the clear button, because the fixture stores a set"
    );
    for field in ["app_key", "app_secret", "access_token"] {
        assert!(
            harness
                .field(&format!("market_participants.longbridge.{field}"))
                .is_none(),
            "the value {field} is edited in the dialog rather than drawn as a row of its own"
        );
    }
}

/// What a user configures on Longbridge's page is what the settings hold: the region moves the
/// site the venue is reached on, and the credential dialog writes the venue's own set.
#[test]
fn longbridges_page_should_write_the_region_and_the_credentials() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.longbridge");
    let kind = CredentialsKind::Longbridge;
    let china = LongbridgeRegion::ALL
        .iter()
        .position(|region| *region == LongbridgeRegion::China)
        .expect("the region table publishes the mainland-China site");

    choose_option(&mut harness, "market_participants.longbridge.region", china);

    open_credentials_dialog(&mut harness, kind);
    for (field, value) in [
        ("app_key", "typed-app-key"),
        ("app_secret", "typed-app-secret"),
        ("access_token", "typed-access-token"),
    ] {
        type_into(&mut harness, kind, field, value);
    }
    confirm_dialog(&mut harness);

    let written = harness.config();
    let Some(VenueSettings::Longbridge(settings)) = written.venue(&VenueId::Longbridge) else {
        panic!("longbridge is configured as Longbridge");
    };
    assert_eq!(
        settings.credentials,
        Some(
            app_config::market_participants::credentials::LongbridgeCredentials {
                app_key: SmolStr::new_static("typed-app-key"),
                app_secret: SmolStr::new_static("typed-app-secret"),
                access_token: SmolStr::new_static("typed-access-token"),
            }
        ),
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

/// A venue page is drawn in the language the interface is rebuilt in: the labels are the catalogue's
/// sentences, not the ones the page was first built with.
#[test]
fn a_venue_page_should_be_drawn_in_the_language_it_is_rebuilt_in() {
    let mut config = configured_settings();
    config.language = app_config::settings::Language::ZhCn;
    let translate = translate_in(LanguageCode::ZhCn);
    let mut harness = Harness::build(config, translate.clone());
    open_page(&mut harness, "market_participants.longbridge");

    assert_eq!(
        harness.field_label("market_participants.longbridge.region"),
        i18n::t_smol!(translate, "settings_ui.market_participants.service_region"),
        "the row is named in the language the page was built in"
    );
    assert_eq!(
        harness.page().current_page().as_str(),
        "market_participants.longbridge",
        "the page itself is still drawn"
    );
}

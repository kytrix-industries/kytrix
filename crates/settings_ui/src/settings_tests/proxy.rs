use std::{io, path::PathBuf, time::Instant};

use app_config::{
    network::{ProxyAuthorizationConfig, ProxyConfig, ProxyRoute, ServiceTarget},
    settings::{Config, Language},
};
use app_config_provider::store::ConfigPersistenceError;
use i18n::language::codes::LanguageCode;
use smol_str::SmolStr;
use ui_core::{
    event::{Event, InputEvent, KeyCode},
    math::px,
    node::id::NodeId,
};

use crate::{
    pages::proxy::editor::{
        ADD_SELECTOR, AUTHORIZATION_SELECTOR, CANCEL_SELECTOR, CONFIRM_SELECTOR, DNS_MODE_SELECTOR,
        EDIT_SELECTOR, FOOTER_SELECTOR,
    },
    session::{SettingsChange, SettingsWriteError},
    settings_tests::test_support::{Harness, configured_settings, translate_in},
};

const PREFIX: &str = "settings.test.server-management.proxy";

const ADD_FORM: &str = "settings.test.server-management.proxy.add-form";

const EDIT_FORM: &str = "settings.test.server-management.proxy.edit-form";

const ADD_DIALOG: &str = "settings.test.server-management.proxy.add-form.editor";

const EDIT_DIALOG: &str = "settings.test.server-management.proxy.edit-form.editor";

const CONFIGURED: &str = "hk";

fn proxy(addr: &'static str, port: u16) -> ProxyConfig {
    ProxyConfig {
        addr: SmolStr::new_static(addr),
        port,
        authorization: ProxyAuthorizationConfig::None,
        resolve_dns_at_remote: false,
    }
}

fn configured() -> Config {
    let mut config = configured_settings();
    config.language = Language::En;
    config
}

fn find_exact(harness: &Harness, id: &str) -> Option<NodeId> {
    let ids = harness.debug_ids();
    let index = ids.iter().position(|debug| debug == id)?;
    harness.find_all("").into_iter().nth(index)
}

fn open_catalog(harness: &mut Harness) {
    harness.settle(2);
    harness.select_page("server-management");
    harness.settle(2);
}

fn open_add_form(harness: &mut Harness) {
    open_catalog(harness);
    harness.click_node("SETTINGS_PROXY_ADD_BUTTON");
    harness.settle(2);
}

fn open_edit_form(harness: &mut Harness, row: usize) {
    open_catalog(harness);
    harness.click_node(&format!("SETTINGS_PROXY_ROW_{row}_edit"));
    harness.settle(2);
}

fn click_field(harness: &mut Harness, field: &str) {
    harness.click_node(&format!("SETTINGS_PROXY_FIELD_{field}"));
    harness.settle(1);
}

fn type_text(harness: &mut Harness, text: &str) {
    harness.type_text(text);
    harness.settle(1);
}

fn type_into(harness: &mut Harness, field: &str, text: &str) {
    click_field(harness, field);
    type_text(harness, text);
}

#[test]
fn confirming_in_the_same_batch_as_the_last_key_should_keep_it() {
    let mut harness = Harness::with_config(configured());
    open_add_form(&mut harness);
    click_field(&mut harness, "name");
    harness.dispatch_without_frame(Event::Input(InputEvent::Text {
        text: SmolStr::new_static("osaka"),
        timestamp: Instant::now(),
    }));
    click_field(&mut harness, "addr");
    harness.dispatch_without_frame(Event::Input(InputEvent::Text {
        text: SmolStr::new_static("127.0.0.1"),
        timestamp: Instant::now(),
    }));
    click_field(&mut harness, "port");
    harness.dispatch_without_frame(Event::Input(InputEvent::Text {
        text: SmolStr::new_static("1080"),
        timestamp: Instant::now(),
    }));
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    let stored = harness.config();
    assert_eq!(
        stored.proxies.get("osaka").map(|proxy| proxy.port),
        Some(1080),
        "confirming must read the key that arrived in the same batch"
    );
}

fn click_alert(harness: &mut Harness, suffix: &str) {
    let target = harness
        .debug_ids()
        .into_iter()
        .find(|id| id.starts_with("alert.") && id.ends_with(&format!(".{suffix}:interactive")))
        .unwrap_or_else(|| panic!("the alert should draw its {suffix} button"));
    harness.click_node(&target);
    harness.settle(2);
}

fn cell_text(harness: &Harness, row: usize, column: usize) -> String {
    let node = harness
        .find(&format!("{PREFIX}.table:cell:{row}:{column}"))
        .unwrap_or_else(|| panic!("the catalog should draw the cell ({row}, {column})"));

    harness.text_of(node)
}

fn header_text(harness: &Harness, column: usize) -> SmolStr {
    let node = harness
        .find(&format!("{PREFIX}.table:header:{column}"))
        .unwrap_or_else(|| panic!("the catalog should draw the header of column {column}"));

    SmolStr::new(harness.text_of(node))
}

fn is_inside(harness: &Harness, node: NodeId, ancestor: NodeId) -> bool {
    let Some(window) = harness.app().window(harness.window()) else {
        return false;
    };
    let tree = window.tree();
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = tree.get(current).ok().and_then(|entry| entry.parent());
    }
    false
}

fn unwritable() -> SettingsWriteError {
    SettingsWriteError::new(ConfigPersistenceError::CreateDirectory {
        path: PathBuf::from("blocked"),
        source: io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the directory cannot be made",
        ),
    })
}

#[test]
fn the_catalog_should_be_drawn_with_its_columns_and_entries() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);

    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "the server-management page should be drawn"
    );
    assert!(
        harness.find("SETTINGS_PROXY_TABLE").is_some(),
        "the catalog should be drawn as a table"
    );

    for (column, header) in [
        "Name",
        "Address",
        "Port",
        "Authentication",
        "Username",
        "Password",
        "DNS mode",
        "",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            header_text(&harness, column),
            header,
            "header of column {column}"
        );
    }

    assert_eq!(cell_text(&harness, 0, 0), CONFIGURED);
    assert_eq!(cell_text(&harness, 0, 1), "127.0.0.1");
    assert_eq!(cell_text(&harness, 0, 2), "1080");
    assert_eq!(cell_text(&harness, 0, 3), "Username and password");
    assert_eq!(cell_text(&harness, 0, 4), "proxy-user");
    assert_eq!(cell_text(&harness, 0, 5), "••••••");
    assert_eq!(cell_text(&harness, 0, 6), "Proxy");
    assert_eq!(cell_text(&harness, 0, 7), "Edit proxy\nDelete proxy");

    let page = harness.find("SETTINGS_PROXY_PAGE").expect("the page");
    assert!(
        !harness.text_of(page).contains("proxy-password"),
        "a stored password must not be drawn back: {}",
        harness.text_of(page)
    );
}

#[test]
fn the_catalog_should_be_drawn_in_name_order_and_not_sorted_by_a_click() {
    let mut config = Config::default();
    config
        .proxies
        .insert(SmolStr::new_static("osaka"), proxy("10.0.0.2", 1081));
    config
        .proxies
        .insert(SmolStr::new_static("hk"), proxy("10.0.0.1", 1080));
    let mut harness = Harness::with_config(config);
    open_catalog(&mut harness);

    assert_eq!(
        cell_text(&harness, 0, 0),
        "hk",
        "the first row is the first name"
    );
    assert_eq!(
        cell_text(&harness, 1, 0),
        "osaka",
        "the second row is the second name"
    );

    harness.click_node(&format!("{PREFIX}.table:header:0"));
    harness.settle(2);

    assert_eq!(
        cell_text(&harness, 0, 0),
        "hk",
        "a header click sorts nothing"
    );
    assert_eq!(
        cell_text(&harness, 1, 0),
        "osaka",
        "a header click sorts nothing"
    );
}

#[test]
fn an_empty_catalog_should_draw_the_message_it_has_to_say() {
    let mut harness = Harness::new();
    open_catalog(&mut harness);

    let message = harness
        .find("SETTINGS_PROXY_EMPTY")
        .expect("an empty catalog should draw the message it holds nothing");
    assert_eq!(harness.text_of(message), "No proxies");
    assert!(
        harness.find(&format!("{PREFIX}.table:cell:0:0")).is_none(),
        "an empty catalog has no rows to draw"
    );

    let table = harness
        .find("SETTINGS_PROXY_TABLE")
        .expect("the table frame");
    let message = harness.bounds(message).expect("the message is laid out");
    let table = harness.bounds(table).expect("the table is laid out");
    assert!(
        (message.origin.x + message.size.width / 2.0 - (table.origin.x + table.size.width / 2.0))
            .abs()
            < px(1.0),
        "the message should be drawn across the whole table: {message:?} in {table:?}"
    );
}

#[test]
fn a_catalog_with_an_entry_should_draw_the_entry_rather_than_the_empty_message() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);

    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "the catalog should still be the page that is drawn"
    );
    assert_eq!(cell_text(&harness, 0, 0), CONFIGURED);
    let message = harness
        .find("SETTINGS_PROXY_EMPTY")
        .expect("the empty overlay");
    assert_eq!(
        harness.text_of(message),
        "",
        "a catalog that holds an entry must not say it holds none"
    );
}

#[test]
fn the_add_button_should_sit_above_a_table_that_fills_the_page() {
    let mut harness = Harness::new();
    open_catalog(&mut harness);

    let page = harness
        .bounds(harness.find("SETTINGS_PROXY_PAGE").expect("the page"))
        .expect("the page is laid out");
    let add = harness
        .bounds(
            harness
                .find("SETTINGS_PROXY_ADD_BUTTON")
                .expect("the button that adds an entry"),
        )
        .expect("the button is laid out");
    let table = harness
        .bounds(
            harness
                .find("SETTINGS_PROXY_TABLE")
                .expect("the table frame"),
        )
        .expect("the table is laid out");
    let content = harness
        .bounds(
            harness
                .find("SETTINGS_CONTENT")
                .expect("the settings content column"),
        )
        .expect("the content column is laid out");

    assert_eq!(
        add.size.height,
        harness.theme().tokens().medium.height,
        "the button that adds an entry is a medium one: {add:?}"
    );
    assert!(
        (add.origin.x + add.size.width - (page.origin.x + page.size.width)).abs() < px(1.0),
        "the button belongs at the right end of the row above the table: {add:?} in {page:?}"
    );
    assert!(
        add.origin.y + add.size.height <= table.origin.y,
        "the table belongs below the button rather than beside it: {table:?} under {add:?}"
    );
    assert!(
        (page.origin.y + page.size.height - (content.origin.y + content.size.height)).abs()
            < px(1.0),
        "the page should be the whole of the body it is drawn in: {page:?} beside {content:?}"
    );
    assert!(
        (table.origin.y + table.size.height - (page.origin.y + page.size.height)).abs() < px(1.0),
        "the table should be the rest of the page: {table:?} in {page:?}"
    );
    assert!(
        (table.size.width - page.size.width).abs() < px(1.0),
        "the table should be as wide as the page it fills: {table:?} in {page:?}"
    );
}

#[test]
fn a_typed_entry_should_reach_the_settings_when_the_form_is_confirmed() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);

    assert!(
        find_exact(&harness, ADD_SELECTOR).is_some(),
        "the form that adds an entry should be drawn over the settings"
    );

    type_into(&mut harness, "name", "osaka");
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "1080");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    assert!(
        harness.find(&format!("{ADD_DIALOG}:panel")).is_none(),
        "confirming the form should take it away"
    );
    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "the settings the form was opened over should still be drawn"
    );
    let stored = harness.config();
    assert_eq!(
        stored.proxies.get("osaka").map(|proxy| proxy.port),
        Some(1080),
        "the entry the user typed should be in the settings: {:?}",
        stored.proxies
    );
}

#[test]
fn a_new_entry_should_resolve_names_here_until_the_form_is_asked_otherwise() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);

    type_into(&mut harness, "name", "osaka");
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "1080");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    assert_eq!(
        harness
            .config()
            .proxies
            .get("osaka")
            .map(|proxy| proxy.resolve_dns_at_remote),
        Some(false),
        "the entry the form recorded should resolve names here"
    );
}

#[test]
fn choosing_the_proxy_mode_should_be_what_the_entry_is_recorded_with() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);

    type_into(&mut harness, "name", "osaka");
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "1080");

    harness.click_node(&format!("{ADD_FORM}.dns-mode:trigger:interactive"));
    harness.settle(2);
    harness.click_node(&format!("{ADD_FORM}.dns-mode:panel:list:row:1"));
    harness.settle(2);
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    assert_eq!(
        harness
            .config()
            .proxies
            .get("osaka")
            .map(|proxy| proxy.resolve_dns_at_remote),
        Some(true),
        "the entry should be recorded with the mode the user chose"
    );
}

#[test]
fn an_edit_form_should_open_on_the_mode_the_entry_is_stored_under() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);

    assert!(
        harness
            .config()
            .proxies
            .get(CONFIGURED)
            .is_some_and(|proxy| proxy.resolve_dns_at_remote),
        "the entry this test edits should be one the proxy resolves names for"
    );

    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_none(),
        "confirming the form should take it away"
    );
    assert!(
        harness
            .config()
            .proxies
            .get(CONFIGURED)
            .is_some_and(|proxy| proxy.resolve_dns_at_remote),
        "confirming the form should record the entry with the mode it opened on"
    );
}

#[test]
fn a_cancelled_form_should_leave_the_settings_alone() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);
    let language = if harness.config().language == Language::En {
        Language::ZhCn
    } else {
        Language::En
    };
    let _ = harness.write(SettingsChange::edit(move |config| {
        config.language = language
    }));
    harness.settle(2);
    let before = harness.config();
    open_edit_form(&mut harness, 0);

    type_text(&mut harness, "osaka");
    harness.click_node(CANCEL_SELECTOR);
    harness.settle(2);

    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_none(),
        "cancelling should take the form away"
    );
    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "the settings the form was opened over should still be drawn"
    );
    assert_eq!(
        harness.page().current_page(),
        "server-management",
        "the settings should still be on the page the form was opened over"
    );
    assert_eq!(
        harness.config().proxies,
        before.proxies,
        "a cancelled form must not write the settings"
    );
    assert_eq!(
        harness.config().language,
        language,
        "a cancelled form must not lose the change that was already stored"
    );
    assert_eq!(
        harness.writes().len(),
        1,
        "only the language change was stored"
    );
}

#[test]
fn the_form_buttons_should_sit_under_a_line_at_the_foot_of_the_form() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);

    let form = harness
        .bounds(find_exact(&harness, EDIT_SELECTOR).expect("the form should be drawn"))
        .expect("the form is laid out");
    let footer = harness
        .bounds(
            harness
                .find(FOOTER_SELECTOR)
                .expect("the form should draw its foot"),
        )
        .expect("the foot is laid out");
    let confirm = harness
        .bounds(harness.find(CONFIRM_SELECTOR).expect("the confirm button"))
        .expect("the confirm button is laid out");
    let cancel = harness
        .bounds(harness.find(CANCEL_SELECTOR).expect("the cancel button"))
        .expect("the cancel button is laid out");
    let field = harness
        .bounds(
            harness
                .find("SETTINGS_PROXY_FIELD_port")
                .expect("the port field"),
        )
        .expect("the port field is laid out");

    assert!(
        field.origin.y + field.size.height <= footer.origin.y,
        "the line the row starts with belongs between the fields and the buttons: {field:?} \
         above {footer:?}"
    );
    assert!(
        footer.origin.y <= cancel.origin.y && footer.origin.y <= confirm.origin.y,
        "the buttons belong under the line rather than over it: {footer:?} over {cancel:?} and \
         {confirm:?}"
    );
    assert!(
        (footer.origin.y + footer.size.height - (form.origin.y + form.size.height)).abs() < px(1.0),
        "the row should be the foot of the form: {footer:?} in {form:?}"
    );
    assert!(
        (confirm.origin.x + confirm.size.width - (form.origin.x + form.size.width)).abs() < px(1.0),
        "the row should be laid out from the right edge of the form: {confirm:?} in {form:?}"
    );
    assert_eq!(
        cancel.size.height, confirm.size.height,
        "the two buttons should be one size: {cancel:?} beside {confirm:?}"
    );
}

#[test]
fn a_form_over_the_settings_should_write_the_entry_at_once() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);

    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_some(),
        "the form should be drawn over the settings"
    );
    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "the settings should still be drawn under the form"
    );

    click_field(&mut harness, "name");
    type_text(&mut harness, "osaka");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    let stored = harness.config();
    assert_eq!(
        stored.proxies.get("hkosaka").map(|proxy| proxy.port),
        Some(1080),
        "confirming the form should write the entry into the settings"
    );
    assert!(
        !stored.proxies.contains_key(CONFIGURED),
        "renaming an entry should move it rather than leave the old name behind"
    );
    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_none(),
        "confirming the form should take it away"
    );
    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "the settings the form was opened over should be the ones left showing"
    );
}

#[test]
fn renaming_an_entry_should_move_every_route_bound_to_it() {
    let mut config = Config::default();
    config
        .proxies
        .insert(SmolStr::new_static(CONFIGURED), proxy("127.0.0.1", 1080));
    config.proxy_default = ProxyRoute::named(CONFIGURED);
    config.proxy_applications.insert(
        ServiceTarget::KytrixService {
            capability: app_config::network::KytrixServiceCapability::XMedia,
        },
        ProxyRoute::named(CONFIGURED),
    );
    let mut harness = Harness::with_config(config);
    open_edit_form(&mut harness, 0);

    click_field(&mut harness, "name");
    type_text(&mut harness, "2");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    let stored = harness.config();
    assert!(
        stored.proxies.contains_key("hk2"),
        "the entry moved: {:?}",
        stored.proxies
    );
    assert_eq!(
        cell_text(&harness, 0, 0),
        "hk2",
        "the catalog the page draws should follow the entry it renamed"
    );
    assert_eq!(
        stored.proxy_default,
        ProxyRoute::named("hk2"),
        "the default line follows the renamed entry"
    );
    assert_eq!(
        stored
            .proxy_applications
            .values()
            .next()
            .and_then(ProxyRoute::named_name),
        Some("hk2"),
        "every route bound to the entry follows it"
    );
}

#[test]
fn a_rejected_form_should_stay_open_and_report_every_problem() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);

    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    let form = find_exact(&harness, ADD_SELECTOR).expect("a rejected form should stay open");
    let text = harness.text_of(form);
    for message in [
        "Enter a proxy name",
        "Enter a host name or an IP address",
        "Port must be an integer from 1 to 65535",
    ] {
        assert!(
            text.contains(message),
            "the form should say {message:?}: {text}"
        );
    }
    assert!(
        harness.config().proxies.is_empty(),
        "a rejected form must not write the settings"
    );
}

#[test]
fn a_name_the_catalog_holds_should_not_be_taken_twice() {
    let mut harness = Harness::with_config(configured());
    open_add_form(&mut harness);

    type_into(&mut harness, "name", CONFIGURED);
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "1080");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    let form = find_exact(&harness, ADD_SELECTOR).expect("a rejected form should stay open");
    let text = harness.text_of(form);
    assert!(
        text.contains("A proxy with this name already exists"),
        "the form should say the name is taken: {text}"
    );
    assert_eq!(
        harness.config().proxies.len(),
        1,
        "a rejected form must not write the settings"
    );
}

#[test]
fn a_port_outside_the_range_should_be_rejected() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);

    type_into(&mut harness, "name", "osaka");
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "0");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    let form = find_exact(&harness, ADD_SELECTOR).expect("a rejected form should stay open");
    assert!(
        harness
            .text_of(form)
            .contains("Port must be an integer from 1 to 65535"),
        "the form should reject the port"
    );
    assert!(
        harness.config().proxies.is_empty(),
        "a rejected form must not write the settings"
    );
}

#[test]
fn an_authenticated_entry_should_be_recorded_with_its_credentials() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);

    type_into(&mut harness, "name", "osaka");
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "1080");
    harness.click_node(&format!("{ADD_FORM}.authorization:trigger:interactive"));
    harness.settle(2);
    harness.click_node(&format!("{ADD_FORM}.authorization:panel:list:row:1"));
    harness.settle(2);
    type_into(&mut harness, "username", "proxy-user");
    type_into(&mut harness, "password", "proxy-password");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    assert_eq!(
        harness
            .config()
            .proxies
            .get("osaka")
            .map(|proxy| proxy.authorization.clone()),
        Some(ProxyAuthorizationConfig::UsernameAndPassword {
            username: SmolStr::new_static("proxy-user"),
            password: SmolStr::new_static("proxy-password"),
        }),
        "the credentials the user typed should be recorded"
    );
}

#[test]
fn a_delete_should_ask_before_it_takes_the_entry_out_of_the_catalog() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);

    harness.click_node("SETTINGS_PROXY_ROW_0_delete");
    harness.settle(2);

    assert!(
        harness.find("alert.").is_some(),
        "a row that asks for a delete should be answered with an alert"
    );
    assert!(
        harness.config().proxies.contains_key(CONFIGURED),
        "the entry should still be in the settings while the alert asking about it is up"
    );

    click_alert(&mut harness, "cancel");

    assert!(
        harness.config().proxies.contains_key(CONFIGURED),
        "walking away from the alert should leave the entry in the settings"
    );
    assert!(
        harness.find("alert.").is_none(),
        "the alert should leave the window once it is dismissed"
    );
}

#[test]
fn a_delete_alert_should_write_the_removal_at_once() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);

    harness.click_node("SETTINGS_PROXY_ROW_0_delete");
    harness.settle(2);
    let alert = harness
        .find("alert.")
        .expect("the alert asking about the delete");
    let text = harness.text_of(alert);
    assert!(
        text.contains(CONFIGURED),
        "the alert should name the entry it is about: {text}"
    );
    assert!(
        !text.contains("<p"),
        "the alert is plain text, so its sentence must not be drawn as markup: {text}"
    );

    click_alert(&mut harness, "ok");

    assert!(
        !harness.config().proxies.contains_key(CONFIGURED),
        "confirming the alert should remove the entry from the settings"
    );
    let empty = harness
        .find("SETTINGS_PROXY_EMPTY")
        .expect("the catalog should draw what it has to say");
    assert_eq!(
        harness.text_of(empty),
        "No proxies",
        "a catalog the delete emptied should say it holds nothing"
    );
    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "removing a row leaves the settings page open"
    );
}

#[test]
fn the_delete_alert_should_name_the_entry_in_both_languages() {
    for language in [LanguageCode::En, LanguageCode::ZhCn] {
        let translate = translate_in(language);
        let title = i18n::t_smol!(translate, "settings_ui.proxies.delete_title");
        let message = i18n::t_smol!(
            translate,
            "settings_ui.proxies.delete_message",
            name = "osaka"
        );

        assert!(
            !title.is_empty() && !message.is_empty(),
            "the title and the message of {language:?} should not be empty strings"
        );
        assert!(
            message.contains("osaka"),
            "the message of {language:?} should name the entry it is about: {message}"
        );
        assert!(
            !message.contains("%{"),
            "every placeholder of {language:?} should be filled in: {message}"
        );
    }
}

#[test]
fn a_form_should_keep_the_settings_under_it_out_of_reach() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);
    let before = harness.config();

    harness.click_node("SETTINGS_ENTRY_data-format");
    harness.settle(2);

    assert_eq!(
        harness.page().current_page(),
        "server-management",
        "the settings should not move to another page from a click aimed through the form"
    );
    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_some(),
        "the form should still be open over the settings"
    );
    assert_eq!(
        harness.config().proxies,
        before.proxies,
        "the click should have been swallowed by the form rather than reached the settings"
    );
}

#[test]
fn a_failed_add_should_keep_the_form_open_and_report() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);
    harness.fail_next_write(unwritable());

    type_into(&mut harness, "name", "osaka");
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "1080");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    assert!(
        find_exact(&harness, ADD_SELECTOR).is_some(),
        "a form whose entry could not be stored should stay open"
    );
    assert!(
        !harness.config().proxies.contains_key("osaka"),
        "settings that could not be stored must not be published as if they had been"
    );
    let all = harness.text_of(harness.root());
    assert!(
        all.contains("The settings could not be saved"),
        "a write that could not be stored should tell the user: {all}"
    );
}

#[test]
fn a_failed_delete_should_keep_the_entry_and_report() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);
    harness.fail_next_write(unwritable());

    harness.click_node("SETTINGS_PROXY_ROW_0_delete");
    harness.settle(2);
    click_alert(&mut harness, "ok");

    assert!(
        harness.config().proxies.contains_key(CONFIGURED),
        "a delete that could not be stored must not remove the entry"
    );
    let all = harness.text_of(harness.root());
    assert!(
        all.contains("The settings could not be saved"),
        "a write that could not be stored should tell the user: {all}"
    );
}

#[test]
fn escape_should_leave_the_form_without_writing_it() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);
    let before = harness.config();

    type_text(&mut harness, "osaka");
    harness.press(KeyCode::Escape);
    harness.settle(2);

    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_none(),
        "escape should take the form away"
    );
    assert_eq!(
        harness.config().proxies,
        before.proxies,
        "escape must not write the settings"
    );
    assert!(
        harness.writes().is_empty(),
        "a form walked away from writes nothing"
    );
}

#[test]
fn tab_should_move_the_focus_inside_the_form() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);
    let panel = harness
        .find(&format!("{EDIT_DIALOG}:panel"))
        .expect("the form should be drawn");

    harness.press(KeyCode::Tab);
    harness.settle(1);
    let first = harness
        .app()
        .focused(harness.window())
        .expect("tab focuses a control");
    assert!(
        is_inside(&harness, first, panel),
        "tab should focus a control of the form, not one of the settings under it"
    );

    harness.press(KeyCode::Tab);
    harness.settle(1);
    let second = harness
        .app()
        .focused(harness.window())
        .expect("tab focuses a control");
    assert_ne!(second, first, "tab should move the focus on");
}

#[test]
fn enter_should_activate_the_confirm_button() {
    let mut harness = Harness::new();
    open_add_form(&mut harness);

    type_into(&mut harness, "name", "osaka");
    type_into(&mut harness, "addr", "127.0.0.1");
    type_into(&mut harness, "port", "1080");
    let confirm = find_exact(&harness, &format!("{ADD_FORM}.confirm:interactive"))
        .expect("the confirm button should be drawn");
    let window = harness.window();
    assert!(
        harness.app_mut().set_focus(window, Some(confirm)),
        "the confirm button should take the focus"
    );
    harness.settle(1);
    harness.press(KeyCode::Enter);
    harness.settle(2);

    assert_eq!(
        harness
            .config()
            .proxies
            .get("osaka")
            .map(|proxy| proxy.port),
        Some(1080),
        "enter on the confirm button should write the entry"
    );
}

#[test]
fn closing_the_form_should_release_its_control_handlers() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);
    let confirm_action =
        ui_core::event::Action::Custom(smol_str::format_smolstr!("{EDIT_FORM}.confirm.click"));
    assert!(
        harness.app().has_action_handler(&confirm_action),
        "an open form's confirm button should have its handler"
    );

    harness.click_node(CANCEL_SELECTOR);
    harness.settle(2);

    assert!(
        !harness.app().has_action_handler(&confirm_action),
        "the form should take its confirm handler with it when it closes"
    );
}

#[test]
fn leaving_the_page_should_release_the_catalog_and_its_dialogs() {
    let mut harness = Harness::with_config(configured());
    open_add_form(&mut harness);
    let add_action =
        ui_core::event::Action::Custom(smol_str::format_smolstr!("{PREFIX}.add.click"));
    let row_action =
        ui_core::event::Action::Custom(smol_str::format_smolstr!("{PREFIX}.row.0.edit"));
    let confirm_action =
        ui_core::event::Action::Custom(smol_str::format_smolstr!("{ADD_FORM}.confirm.click"));
    assert!(
        harness.app().has_action_handler(&add_action),
        "the page's add button should have its handler"
    );
    assert!(
        harness.app().has_action_handler(&row_action),
        "a catalog row should have its handler"
    );
    assert!(
        harness.app().has_action_handler(&confirm_action),
        "the open form's confirm button should have its handler"
    );

    harness.press(KeyCode::Escape);
    harness.settle(2);
    assert!(
        !harness.app().has_action_handler(&confirm_action),
        "the form should take its handlers with it when it closes"
    );

    harness.select_page("appearance");
    harness.settle(2);

    assert!(
        !harness.app().has_action_handler(&add_action),
        "the page should take its add handler with it"
    );
    assert!(
        !harness.app().has_action_handler(&row_action),
        "the page should take its row handlers with it"
    );
    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_none(),
        "the page's nodes should be gone"
    );
}

#[test]
fn the_page_should_be_reachable_through_the_search() {
    let mut harness = Harness::new();
    harness.set_query("proxy server");

    assert!(
        harness
            .page()
            .current_page()
            .as_str()
            .eq("server-management"),
        "a search for the page should open it: {}",
        harness.page().current_page()
    );
}

#[test]
fn a_form_should_be_drawn_by_the_frame_the_click_asks_for() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);

    harness.click_node("SETTINGS_PROXY_ROW_0_edit");
    harness.settle(1);

    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_some(),
        "the click alone should be enough for the form to be drawn"
    );
}

#[test]
fn the_form_should_draw_the_choices_an_entry_stores() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);

    assert!(
        harness.find(AUTHORIZATION_SELECTOR).is_some(),
        "the form should draw the control the authentication is chosen in"
    );
    assert!(
        harness.find(DNS_MODE_SELECTOR).is_some(),
        "the form should draw the control the DNS mode is chosen in"
    );
    let form = find_exact(&harness, EDIT_SELECTOR).expect("the form");
    let text = harness.text_of(form);
    for label in [
        "Name",
        "Socks5 address",
        "Socks5 port",
        "Socks5 authentication",
    ] {
        assert!(
            text.contains(label),
            "the form should draw {label:?}: {text}"
        );
    }
}

#[test]
fn an_edit_form_should_start_from_the_entry_it_edits() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);

    let form = find_exact(&harness, EDIT_SELECTOR).expect("the form");
    let text = harness.text_of(form);
    assert!(
        text.contains("127.0.0.1"),
        "the address should be filled in: {text}"
    );
    assert!(
        text.contains("1080"),
        "the port should be filled in: {text}"
    );
    assert!(
        text.contains("proxy-user"),
        "the username should be filled in: {text}"
    );
    assert!(
        harness
            .find(&format!("{EDIT_FORM}.field.name.root"))
            .is_some(),
        "the form should draw the entry's name field"
    );
    assert_eq!(
        harness.config().proxies.len(),
        1,
        "opening the form writes nothing"
    );
}

#[test]
fn each_row_should_ask_for_its_own_entry() {
    let mut config = Config::default();
    config
        .proxies
        .insert(SmolStr::new_static("hk"), proxy("10.0.0.1", 1080));
    config
        .proxies
        .insert(SmolStr::new_static("osaka"), proxy("10.0.0.2", 1081));
    let mut harness = Harness::with_config(config);
    open_catalog(&mut harness);

    harness.click_node("SETTINGS_PROXY_ROW_1_delete");
    harness.settle(2);
    let alert = harness
        .find("alert.")
        .expect("the alert asking about the delete");
    let text = harness.text_of(alert);
    assert!(
        text.contains("osaka"),
        "the alert should name the row that was clicked: {text}"
    );

    click_alert(&mut harness, "cancel");

    assert_eq!(
        harness.config().proxies.len(),
        2,
        "walking away leaves both entries"
    );
}

#[test]
fn a_language_change_should_redraw_the_catalog() {
    let mut harness = Harness::with_config(configured());
    open_catalog(&mut harness);

    harness.update_config(|config| config.language = Language::ZhCn);
    harness.settle(2);

    assert_eq!(header_text(&harness, 0), "名称");
    assert_eq!(cell_text(&harness, 0, 0), CONFIGURED);
    harness.click_node("SETTINGS_PROXY_ROW_0_edit");
    harness.settle(2);
    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_some(),
        "the rebuilt page should still open the form its rows ask for"
    );
}

#[test]
fn opening_and_closing_the_form_should_leave_one_form_behind() {
    let mut harness = Harness::with_config(configured());

    for round in 0..4 {
        open_edit_form(&mut harness, 0);
        click_field(&mut harness, "name");
        type_text(&mut harness, &format!("{round}"));
        harness.click_node(CANCEL_SELECTOR);
        harness.settle(2);
        assert!(
            harness.find(&format!("{EDIT_DIALOG}:panel")).is_none(),
            "round {round} should close the form it opened"
        );
    }

    open_edit_form(&mut harness, 0);
    click_field(&mut harness, "name");
    type_text(&mut harness, "osaka");
    harness.click_node(CONFIRM_SELECTOR);
    harness.settle(2);

    assert_eq!(
        harness
            .config()
            .proxies
            .get("hkosaka")
            .map(|proxy| proxy.port),
        Some(1080),
        "the form opened after four others should write the entry it shows"
    );
}

#[test]
fn a_language_change_should_release_the_form_the_page_owned() {
    let mut harness = Harness::with_config(configured());
    open_edit_form(&mut harness, 0);
    let confirm_action =
        ui_core::event::Action::Custom(smol_str::format_smolstr!("{EDIT_FORM}.confirm.click"));
    assert!(
        harness.app().has_action_handler(&confirm_action),
        "the open form's confirm button should have its handler"
    );

    harness.update_config(|config| config.language = Language::ZhCn);
    harness.settle(2);

    assert!(
        harness.find(&format!("{EDIT_DIALOG}:panel")).is_none(),
        "the form should not be left over the rebuilt page"
    );
    assert!(
        !harness.app().has_action_handler(&confirm_action),
        "the form's handlers should be released with the page"
    );
    assert!(
        harness.find("SETTINGS_PROXY_PAGE").is_some(),
        "the rebuilt page should be the catalog again"
    );
}

#[test]
fn an_entry_without_authentication_should_draw_no_credentials() {
    let mut config = Config::default();
    config.language = Language::En;
    config
        .proxies
        .insert(SmolStr::new_static("hk"), proxy("10.0.0.1", 1080));
    let mut harness = Harness::with_config(config);
    open_catalog(&mut harness);

    assert_eq!(cell_text(&harness, 0, 3), "No authentication");
    assert_eq!(cell_text(&harness, 0, 4), "", "no username is stored");
    assert_eq!(cell_text(&harness, 0, 5), "", "no password is stored");
    assert_eq!(cell_text(&harness, 0, 6), "Local");
}

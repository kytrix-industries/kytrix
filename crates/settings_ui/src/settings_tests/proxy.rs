//! The tests of the proxy catalog: the form on the window layer and the table's rows.

use std::fs;

use app_config::{
    network::{KytrixServiceCapability, ProxyConfig, ProxyRoute, ServiceTarget},
    settings::{Config, Language},
};
use base_ui::size::UnifiedSize;
use gpui_kit::{TestAppContext, px};
use i18n::{language::codes::LanguageCode, t};
use smol_str::SmolStr;

use super::test_support::*;

/// A form the user walks away from is not a change of the settings.
///
/// The form was opened over the settings, which were never taken off the layer: what
/// cancelling leaves behind is the page that was always there, uncovered rather than put
/// back, and what the form held is dropped rather than written.
///
/// A change has already been stored when the form opens, so what this asks is that leaving
/// the form neither records the entry nor loses the change, and the assertions on the panel
/// are what tell the panel that was always there from one built in its place: a panel built
/// again would open on the default page with an empty search.
#[gpui_kit::test]
fn a_cancelled_form_should_leave_the_settings_alone(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    edit_settings(&panel, |config| config.language = Language::ZhCn, cx);
    select(&panel, "market_participants.okx", cx);
    panel.update(cx, |panel, cx| {
        panel.set_query("market_participants.okx".into(), cx);
    });
    settle(cx);
    let before = stored(cx);
    let selected = current_page(&panel, cx);

    open_edit_form(&panel, "hk", cx);
    type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
    click(cx, crate::pages::proxy::editor::CANCEL_SELECTOR);

    assert!(
        cx.debug_bounds("SETTINGS_PROXY_EDIT").is_none(),
        "cancelling should take the form away"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "the settings the form was opened over should still be drawn"
    );
    assert_eq!(
        stored(cx).proxies,
        before.proxies,
        "a cancelled form must not write the settings"
    );
    assert_eq!(
        stored(cx).language,
        Language::ZhCn,
        "a cancelled form must not lose the change that was already stored"
    );
    assert_eq!(
        current_page(&panel, cx),
        selected,
        "the settings should still be on the page the form was opened over"
    );
    panel.read_with(cx, |panel, _| {
        assert_eq!(
            panel.query_for_test().as_ref(),
            "market_participants.okx",
            "the settings should still hold the search they were found by"
        );
    });
}

/// The form ends the way the application's other dialogs end: a line across it, and the two
/// buttons under that line rather than in among the fields. What the line is for is telling
/// what the form holds from what the form does, so the fields are above it and the buttons are
/// below it, and both buttons are one size because neither is the field's own control.
#[gpui_kit::test]
fn the_form_buttons_should_sit_under_a_separator_at_the_foot_of_the_form(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    open_edit_form(&panel, "hk", cx);

    let form = cx
        .debug_bounds("SETTINGS_PROXY_EDIT")
        .expect("the form should be drawn");
    let footer = cx
        .debug_bounds(crate::pages::proxy::editor::FOOTER_SELECTOR)
        .expect("the form should draw the row its buttons are in");
    let confirm = cx
        .debug_bounds(crate::pages::proxy::editor::CONFIRM_SELECTOR)
        .expect("the form should draw the button that records the entry");
    let cancel = cx
        .debug_bounds(crate::pages::proxy::editor::CANCEL_SELECTOR)
        .expect("the form should draw the button that leaves the entry alone");
    let field = cx
        .debug_bounds("SETTINGS_PROXY_FIELD_port")
        .expect("the form should draw the field the entry's port is typed into");

    assert!(
        field.bottom() <= footer.top(),
        "the line the row starts with belongs between the fields and the buttons: {field:?} \
         above {footer:?}"
    );
    assert!(
        footer.top() <= cancel.top() && footer.top() <= confirm.top(),
        "the buttons belong under the line rather than over it: {footer:?} over {cancel:?} and \
         {confirm:?}"
    );
    assert_eq!(
        footer.bottom(),
        form.bottom(),
        "the row should be the foot of the form: {footer:?} in {form:?}"
    );
    assert!(
        (confirm.right() - form.right()).abs() < px(1.0),
        "the row should be laid out from the right edge of the form: {confirm:?} in {form:?}"
    );
    assert_eq!(
        cancel.size.height, confirm.size.height,
        "the two buttons should be one size, which is neither of them being a control of a \
         field: {cancel:?} beside {confirm:?}"
    );
}

/// The proxy catalog is part of the settings: a row asks for a delete, the user says so in
/// the alert that asks, and the entry is gone at once.
#[gpui_kit::test]
fn a_delete_alert_should_write_the_removal_at_once(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    request_delete(&panel, "hk", cx);
    click(cx, "BASE_UI_ALERT_OK");

    assert!(
        !stored(cx).proxies.contains_key("hk"),
        "confirming the alert should remove the entry from the settings"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "removing a row leaves the settings page open"
    );
}

/// A delete is the one row action that takes something away, so it is asked about before it is
/// written: the alert is where the user hears that the routes naming the entry go with it, and
/// a user who walks away from it leaves the catalog exactly as it was.
#[gpui_kit::test]
fn a_delete_should_ask_before_it_takes_the_entry_out_of_the_catalog(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    request_delete(&panel, "hk", cx);

    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "a row that asks for a delete should be answered with an alert rather than the entry \
         going at once"
    );
    assert!(
        stored(cx).proxies.contains_key("hk"),
        "the entry should still be in the settings while the alert asking about it is up"
    );

    click(cx, "BASE_UI_ALERT_CANCEL");

    assert!(
        stored(cx).proxies.contains_key("hk"),
        "walking away from the alert should leave the entry in the settings"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_none(),
        "the alert should leave the window once it is dismissed"
    );
}

/// The alert a delete raises names the entry it is about, in either language.
///
/// The name is the one thing that tells two rows of the catalog apart, so a message that
/// dropped it would ask the user to confirm a deletion without saying which entry goes. Both
/// locales therefore have to carry the placeholder the name is written into, and the name has
/// to arrive in the text the alert is given.
#[gpui_kit::test]
fn the_delete_alert_should_name_the_entry_in_both_languages(cx: &mut TestAppContext) {
    cx.update(|cx| {
        i18n::global::embedded::install(cx).expect("embedded locales should load");
    });

    for language in [LanguageCode::En, LanguageCode::ZhCn] {
        cx.update(|cx| {
            cx.set_global(language);
        });

        let (declared, title, message) = cx.read(|cx| {
            let i18n = cx.global::<i18n::translator::types::I18n>();

            (
                i18n.try_translate(language, "settings_ui.proxies.delete_message")
                    .is_some(),
                t!(cx, "settings_ui.proxies.delete_title"),
                t!(cx, "settings_ui.proxies.delete_message", name = "osaka"),
            )
        });

        assert!(
            declared,
            "both locales should define the message a delete is confirmed by"
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

/// The form over the settings is a dialog of its own on the same session, drawn over them:
/// the settings stay on the layer while the form is open, and what takes the form away is the
/// form being dismissed rather than the settings being mounted again under it.
///
/// Confirming the form writes its entry into the settings at once; the page under it is the
/// one that was always there and is still the one left showing.
#[gpui_kit::test]
fn a_form_over_the_settings_should_write_the_entry_at_once(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    open_edit_form(&panel, "hk", cx);

    assert!(
        cx.debug_bounds("SETTINGS_PROXY_EDIT").is_some(),
        "the form should be drawn over the settings"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "the settings should still be drawn under the form"
    );

    type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
    click(cx, crate::pages::proxy::editor::CONFIRM_SELECTOR);

    assert_eq!(
        stored(cx).proxies.get("hkosaka").map(|proxy| proxy.port),
        Some(1080),
        "confirming the form should write the entry into the settings"
    );
    assert!(
        !stored(cx).proxies.contains_key("hk"),
        "renaming an entry should move it rather than leave the old name behind"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PROXY_EDIT").is_none(),
        "confirming the form should take it away"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "the settings the form was opened over should be the ones left showing"
    );
}

/// An entry the user types into the form and confirms is written at once.
///
/// What the user typed is in the field and nowhere else: a field reports itself when the
/// caret leaves it, and the confirm button does not take the caret away, so the form has to
/// read the fields back. This is the whole of adding a proxy, from the button to the file.
#[gpui_kit::test]
fn a_typed_entry_should_reach_the_settings_when_the_form_is_confirmed(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);
    open_add_form(&panel, cx);

    type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
    type_into(cx, "SETTINGS_PROXY_FIELD_addr", "127.0.0.1");
    type_into(cx, "SETTINGS_PROXY_FIELD_port", "1080");
    click(cx, crate::pages::proxy::editor::CONFIRM_SELECTOR);

    assert!(
        cx.debug_bounds("SETTINGS_PROXY_ADD").is_none(),
        "confirming a form should take it away"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "the settings the form was opened over should still be drawn when the form is confirmed"
    );
    let written = stored(cx);
    assert_eq!(
        written.proxies.get("osaka").map(|proxy| proxy.port),
        Some(1080),
        "the entry the user typed should be in the settings: {:#?}",
        written.proxies
    );
}

/// A new entry resolves names on this side of the proxy until the form is asked for the other
/// mode: the control opens on the mode the entry is recorded with, so what a user who touches
/// nothing gets is the default and not whatever the form happened to hold.
#[gpui_kit::test]
fn a_new_entry_should_resolve_names_here_until_the_form_is_asked_otherwise(
    cx: &mut TestAppContext,
) {
    let (panel, cx) = settings(Config::default(), cx);
    open_add_form(&panel, cx);

    type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
    type_into(cx, "SETTINGS_PROXY_FIELD_addr", "127.0.0.1");
    type_into(cx, "SETTINGS_PROXY_FIELD_port", "1080");
    click(cx, crate::pages::proxy::editor::CONFIRM_SELECTOR);

    assert_eq!(
        stored(cx)
            .proxies
            .get("osaka")
            .map(|proxy| proxy.resolve_dns_at_remote),
        Some(false),
        "the entry the form recorded should resolve names here"
    );
}

/// The mode the control shows is the mode the entry is recorded with, and the other choice of
/// it is what a user picks to have the proxy resolve the names.
#[gpui_kit::test]
fn choosing_the_proxy_mode_should_be_what_the_entry_is_recorded_with(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);
    open_add_form(&panel, cx);

    type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
    type_into(cx, "SETTINGS_PROXY_FIELD_addr", "127.0.0.1");
    type_into(cx, "SETTINGS_PROXY_FIELD_port", "1080");
    click(cx, crate::pages::proxy::editor::DNS_MODE_SELECTOR);
    assert!(
        cx.debug_bounds("TEST_MENU_ITEM_proxy").is_some(),
        "the control should offer the mode that resolves names there"
    );
    click(cx, "TEST_MENU_ITEM_proxy");
    click(cx, crate::pages::proxy::editor::CONFIRM_SELECTOR);

    assert_eq!(
        stored(cx)
            .proxies
            .get("osaka")
            .map(|proxy| proxy.resolve_dns_at_remote),
        Some(true),
        "the entry should be recorded with the mode the user chose"
    );
}

/// The form that changes an entry is opened on the mode the entry is stored under: a form that
/// started from the default would record the entry as a mode the user never chose, which is the
/// one way an edit the user made nowhere could still change the settings.
#[gpui_kit::test]
fn an_edit_form_should_open_on_the_mode_the_entry_is_stored_under(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    assert!(
        stored(cx)
            .proxies
            .get("hk")
            .is_some_and(|proxy| proxy.resolve_dns_at_remote),
        "the entry this test edits should be one the proxy resolves names for"
    );

    open_edit_form(&panel, "hk", cx);
    click(cx, crate::pages::proxy::editor::CONFIRM_SELECTOR);

    assert!(
        stored(cx)
            .proxies
            .get("hk")
            .is_some_and(|proxy| proxy.resolve_dns_at_remote),
        "confirming the form should record the entry with the mode it opened on"
    );
}

/// A venue's line is a row a user can set before any proxy exists: the control is operable
/// with an empty catalog, and the lines it offers are the row's own choices — the default
/// line among them.
///
/// Every venue starts on the default line, so a row that was only operable once the catalog
/// held an entry would be one a fresh installation cannot put back on it.
#[gpui_kit::test]
fn a_proxy_line_should_be_settable_with_an_empty_catalog(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);
    assert!(
        stored(cx).proxies.is_empty(),
        "the settings this test opens hold no proxy at all"
    );

    select(&dialog, "configure", cx);

    click(cx, "SETTINGS_CONTROL_configure.binance");

    assert!(
        cx.debug_bounds("TEST_DROPDOWN_PANEL").is_some(),
        "a venue's line opens with nothing in the catalog: a disabled control opens nothing"
    );
    // An entry is drawn by a button the menu names after the entry's own id, so the lines the
    // open menu offers are read back by the tokens the settings store those lines under.
    assert!(
        cx.debug_bounds("TEST_MENU_ITEM_@inherit").is_some(),
        "the row offers the default line, which is the line every venue starts on"
    );
    assert!(
        cx.debug_bounds("TEST_MENU_ITEM_@direct").is_some(),
        "beside the one that names no proxy at all"
    );
}

/// The document's default line has nothing above it to inherit from, so its row offers the two
/// built-in lines and the catalog's entries, and writes the one it is given.
#[gpui_kit::test]
fn the_default_line_should_be_set_from_its_row(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "configure", cx);

    click(cx, "SETTINGS_CONTROL_proxy.default-line");
    assert!(
        cx.debug_bounds("TEST_MENU_ITEM_@inherit").is_none(),
        "the default line cannot follow a default above it"
    );
    click(cx, "TEST_MENU_ITEM_@direct");

    assert_eq!(
        stored(cx).proxy_default,
        ProxyRoute::Direct,
        "choosing the direct line stores it as the document's default"
    );
}

/// The Kytrix cloud row stands for every Kytrix service, so one choice reaches all of them.
#[gpui_kit::test]
fn the_kytrix_cloud_row_should_bind_every_service_it_names(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "configure", cx);

    click(cx, "SETTINGS_CONTROL_configure.kytrix.cloud");
    click(cx, "TEST_MENU_ITEM_@direct");

    let stored = stored(cx);
    for capability in [
        KytrixServiceCapability::Account,
        KytrixServiceCapability::News,
        KytrixServiceCapability::X,
        KytrixServiceCapability::PentagonPizzaIndex,
        KytrixServiceCapability::GlobalShippingChokepoint,
    ] {
        assert_eq!(
            stored
                .proxy_applications
                .get(&ServiceTarget::KytrixService { capability }),
            Some(&ProxyRoute::Direct),
            "the cloud line should reach {capability:?}"
        );
    }
}

/// The proxy catalog is drawn by the table rather than by the page's own fields, and a
/// catalog with nothing in it says so once, across the body, rather than in every column
/// of a row that has no entry behind it.
#[gpui_kit::test]
fn the_proxy_page_should_be_drawable_with_an_empty_catalog(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);

    select(&dialog, "server-management", cx);

    dialog.read_with(cx, |dialog, cx| {
        assert_eq!(dialog.current_page_for_test(), "server-management");
        assert_eq!(
            base_ui::table::TableDataSet::<6>::rows_count(dialog.proxies_for_test().read(cx)),
            0,
            "a catalog with nothing in it has no rows for the table to draw"
        );
    });

    let message = cx
        .debug_bounds("SETTINGS_PROXY_EMPTY")
        .expect("the empty catalog should draw the message it has to say");
    let table = cx
        .debug_bounds("SETTINGS_PROXY_TABLE")
        .expect("the page should draw the frame the table is drawn in");
    assert!(
        (message.center().x - table.center().x).abs() < px(1.0),
        "the message should be drawn across the whole table rather than inside one of its \
         columns: it is {message:?} in {table:?}"
    );
}

/// The way to add an entry is the right end of the row above the catalog, and the catalog
/// below that row is the rest of the page: the page is as tall as the dialog gives it, so the
/// table is not a height of its own but whatever the row above it left.
#[gpui_kit::test]
fn the_add_button_should_sit_above_a_table_that_fills_the_page(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);
    select(&dialog, "server-management", cx);

    let page = cx
        .debug_bounds("SETTINGS_PROXY_PAGE")
        .expect("the proxy page should be drawn");
    let add = cx
        .debug_bounds("SETTINGS_PROXY_ADD_BUTTON")
        .expect("the page should draw the button that adds an entry");
    let table = cx
        .debug_bounds("SETTINGS_PROXY_TABLE")
        .expect("the page should draw the frame the table is drawn in");
    // The sidebar is the foot of the column beside the page, so where it ends is where the
    // body the page is drawn in ends: a page that stopped at its own two rows would end
    // higher.
    let column = cx
        .debug_bounds("SETTINGS_SIDEBAR")
        .expect("the dialog should draw the sidebar beside the page");

    assert_eq!(
        add.size.height,
        UnifiedSize::Medium.h(),
        "the button that adds an entry is a medium one: it is {add:?}"
    );
    assert!(
        (add.right() - page.right()).abs() < px(1.0),
        "the button belongs at the right end of the row above the table: it is {add:?} in \
         {page:?}"
    );
    assert!(
        add.bottom() <= table.top(),
        "the table belongs below the button rather than beside it: {table:?} under {add:?}"
    );
    assert_eq!(
        page.bottom(),
        column.bottom(),
        "the page should be the whole of the body it is drawn in: {page:?} beside {column:?}"
    );
    assert_eq!(
        table.bottom(),
        page.bottom(),
        "the table should be the rest of the page: {table:?} in {page:?}"
    );
    assert_eq!(
        table.size.width, page.size.width,
        "the table should be as wide as the page it fills: {table:?} in {page:?}"
    );
}

/// An entry is a row of the catalog: the table counts it, and the body draws it instead of
/// the message that says there is nothing in the catalog.
#[gpui_kit::test]
fn a_catalog_with_an_entry_should_draw_the_entry_rather_than_the_empty_message(
    cx: &mut TestAppContext,
) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "server-management", cx);

    panel.read_with(cx, |dialog, cx| {
        assert_eq!(
            base_ui::table::TableDataSet::<6>::rows_count(dialog.proxies_for_test().read(cx)),
            1,
            "the entry the catalog holds should be the one row of the table"
        );
    });
    assert!(
        cx.debug_bounds("SETTINGS_PROXY_PAGE").is_some(),
        "the catalog should still be the page that is drawn, so that the message being gone \
         is the table's doing rather than the page's"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PROXY_EMPTY").is_none(),
        "a catalog that holds an entry must not draw the message that it holds none"
    );
}

/// The form is drawn over the settings and takes the mouse from them: a click aimed at the
/// settings under it reaches the form instead, so they are out of reach while it is up.
///
/// What draws the form is the frame the request for it asks for: nothing here refreshes the
/// window, so a form that reached the layer without a frame ever drawing it would leave the
/// click to land on the settings underneath.
#[gpui_kit::test]
fn a_form_should_keep_the_settings_under_it_out_of_reach(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    open_edit_form(&panel, "hk", cx);
    let before = stored(cx);
    let page = current_page(&panel, cx);

    let entry = cx
        .debug_bounds("SETTINGS_ENTRY_server-management")
        .expect("the sidebar should draw the row the click is aimed at");
    let form = cx
        .debug_bounds("SETTINGS_PROXY_EDIT")
        .expect("the request alone should be enough for the form over the settings to be drawn");
    assert!(
        !form.contains(&entry.center()),
        "the row the click is aimed at should be outside the form's own panel, or a swallowed \
         click would prove nothing: the form is {form:?} and the row is {entry:?}"
    );

    click(cx, "SETTINGS_ENTRY_server-management");

    assert_eq!(
        current_page(&panel, cx),
        page,
        "the settings should not move to another page from a click aimed through the form"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PROXY_EDIT").is_some(),
        "the form should still be open over the settings"
    );
    assert_eq!(
        stored(cx),
        before,
        "the click should have been swallowed by the form rather than reached the settings"
    );
}

/// A form whose entry could not be stored stays open with what was typed and reports why; once
/// the settings can be stored, confirming it again writes the entry.
#[gpui_kit::test]
fn a_failed_proxy_add_should_keep_the_form_open_and_report(cx: &mut TestAppContext) {
    let (manager, blocking, _storage) = manager_behind_a_file(cx);
    let (panel, cx) = settings_over(manager, Language::En, cx);
    open_add_form(&panel, cx);

    type_into(cx, "SETTINGS_PROXY_FIELD_name", "osaka");
    type_into(cx, "SETTINGS_PROXY_FIELD_addr", "127.0.0.1");
    type_into(cx, "SETTINGS_PROXY_FIELD_port", "1080");
    click(cx, crate::pages::proxy::editor::CONFIRM_SELECTOR);

    assert!(
        cx.debug_bounds("SETTINGS_PROXY_ADD").is_some(),
        "a form whose entry could not be stored should stay open"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "a write that could not be stored should tell the user"
    );
    assert!(
        !stored(cx).proxies.contains_key("osaka"),
        "settings that could not be stored must not be published as if they had been"
    );

    click(cx, "BASE_UI_ALERT_OK");
    fs::remove_file(&blocking).expect("the blocking file should be removable");
    click(cx, crate::pages::proxy::editor::CONFIRM_SELECTOR);

    assert!(
        cx.debug_bounds("SETTINGS_PROXY_ADD").is_none(),
        "the retry should take the form away"
    );
    assert_eq!(
        stored(cx).proxies.get("osaka").map(|proxy| proxy.port),
        Some(1080),
        "the retry should store the entry the user typed"
    );
}

/// A delete that could not be stored keeps the entry and reports why; once the settings can be
/// stored, confirming the same delete removes it.
#[gpui_kit::test]
fn a_failed_proxy_delete_should_keep_the_entry_and_report(cx: &mut TestAppContext) {
    let (mut manager, blocking, _storage) = manager_behind_a_file(cx);
    cx.update(|cx| {
        manager.update(
            |config| {
                config.proxies.insert(
                    SmolStr::new_static("hk"),
                    ProxyConfig {
                        addr: SmolStr::new_static("127.0.0.1"),
                        port: 1080,
                        ..ProxyConfig::default()
                    },
                );
            },
            cx,
        );
    });
    let (panel, cx) = settings_over(manager, Language::En, cx);

    request_delete(&panel, "hk", cx);
    click(cx, "BASE_UI_ALERT_OK");

    assert!(
        stored(cx).proxies.contains_key("hk"),
        "a delete that could not be stored must not remove the entry"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "a write that could not be stored should tell the user"
    );

    click(cx, "BASE_UI_ALERT_OK");
    fs::remove_file(&blocking).expect("the blocking file should be removable");

    request_delete(&panel, "hk", cx);
    click(cx, "BASE_UI_ALERT_OK");

    assert!(
        !stored(cx).proxies.contains_key("hk"),
        "the retry should remove the entry"
    );
}

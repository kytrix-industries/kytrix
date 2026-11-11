//! The tests of writing the settings: at once, after a failure, and on the way out.

use std::fs;

use app_config::{
    market_participants::credentials::OkxCredentials,
    settings::{Appearance, Config, Language},
    ui::SidebarMode,
};
use app_config_provider::{manager::ConfigManager, store::ConfigPersistenceError};
use gpui_kit::TestAppContext;
use i18n::{language::codes::LanguageCode, t};
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use super::test_support::*;
use crate::session::SettingsChange;

/// The alert a rejected commit raises says what went wrong, in either language.
///
/// The message is only worth raising if the user can read the reason, so both locales have
/// to carry the placeholder the reason is written into, and the reason has to arrive in the
/// text the alert is given.
///
/// The copy is centered when it is drawn, which it says with a `<p align="center">` tag pair:
/// that pair is how the message opens and how it ends, so both readings here skip it and ask
/// about the sentence inside it.
///
/// A key that is missing from a locale translates to an empty string rather than failing,
/// which is what makes "the message is not empty" the thing this has to check: a typo in a
/// key would otherwise reach the user as an alert with nothing in it.
#[gpui_kit::test]
fn the_failure_alert_should_name_the_reason_in_both_languages(cx: &mut TestAppContext) {
    let (mut manager, _blocking, _directory) = manager_behind_a_file(cx);
    let mut candidate = manager.read().as_ref().clone();
    candidate.appearance = Appearance::Dark;
    let reason: ConfigPersistenceError = cx.update(|cx| {
        manager
            .write(candidate, cx)
            .expect_err("storing behind a file should fail")
    });

    cx.update(|cx| {
        i18n::global::embedded::install(cx).expect("embedded locales should load");
    });

    for (language, title, opening) in [
        (
            LanguageCode::En,
            "The settings could not be saved",
            "The settings were not written to disk: ",
        ),
        (LanguageCode::ZhCn, "设置保存失败", "设置未能写入磁盘："),
    ] {
        cx.update(|cx| {
            cx.set_global(language);
        });

        let (declared, drawn_title, message, filling) = cx.read(|cx| {
            let i18n = cx.global::<i18n::translator::types::I18n>();

            (
                i18n.try_translate(language, "settings_ui.save_failed.message")
                    .is_some(),
                t!(cx, "settings_ui.save_failed.title"),
                t!(cx, "settings_ui.save_failed.message", reason = reason),
                // The same message with a reason that is a word of its own, which is what
                // says the placeholder of this locale was filled rather than dropped.
                t!(cx, "settings_ui.save_failed.message", reason = "the-reason"),
            )
        });

        assert!(
            declared,
            "both locales should define the message a failed commit raises"
        );
        assert!(
            !drawn_title.is_empty(),
            "the title of {language:?} should not be an empty string"
        );
        assert!(
            !message.is_empty(),
            "the message of {language:?} should not be an empty string"
        );
        // The message is centered markdown, so the reason is followed by the align tag pair's
        // closing tag rather than by the end of the sentence.
        assert!(
            filling.contains("the-reason</p>"),
            "the message of {language:?} should write the reason where its placeholder is: {filling}"
        );
        assert_eq!(
            drawn_title.as_ref(),
            title,
            "the title should be written in {language:?}"
        );
        assert!(
            message
                .trim_start_matches("<p align=\"center\">")
                .starts_with(opening),
            "the message should be written in {language:?}: {message}"
        );
        assert!(
            message.contains(&reason.to_string()),
            "the message should carry the reason {reason}: {message}"
        );
        assert!(
            !message.contains("%{"),
            "every placeholder should be filled in: {message}"
        );
    }
}

/// A write that could not be stored is not one the settings treat as stored: once the
/// reason is gone, the same change reaches the file.
///
/// What this is about is the retry: the file was never written, so the settings in memory
/// are the ones the user started with and the page still holds their choice — asking again
/// has to store it rather than report it as already stored.
#[gpui_kit::test]
fn a_write_that_could_not_be_stored_should_be_stored_when_it_is_tried_again(
    cx: &mut TestAppContext,
) {
    let (manager, blocking, _directory) = manager_behind_a_file(cx);
    let path = blocking.join("config.json");
    let (panel, cx) = settings_over(manager, Language::En, cx);

    edit_settings(&panel, |config| config.appearance = Appearance::Dark, cx);

    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "the failure should be reported before anything is tried again"
    );
    assert!(
        !path.exists(),
        "a write that failed must not have left a file behind: {path:?}"
    );
    assert_eq!(
        stored(cx).appearance,
        Appearance::System,
        "a write that failed must not change the settings in memory"
    );

    // Reading the alert is what leaves the page to be asked again.
    click(cx, "BASE_UI_ALERT_OK");
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_none(),
        "the alert should be gone once it has been read"
    );

    fs::remove_file(&blocking).expect("the blocking file should be removable");

    edit_settings(&panel, |config| config.appearance = Appearance::Dark, cx);

    assert_eq!(
        stored(cx).appearance,
        Appearance::Dark,
        "the retry should store what the user chose"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "the retry should leave the page open"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_none(),
        "a stored change is nothing to report"
    );

    let written = fs::read_to_string(&path).expect("the retry should have written a file");
    assert!(
        written.contains("\"appearance\": \"Dark\""),
        "the file should hold what the user chose: {written}"
    );
    let reloaded = cx
        .update(|_, cx| ConfigManager::from_path(&path, cx))
        .expect("the stored settings should load");
    assert_eq!(
        reloaded.read().as_ref(),
        &stored(cx),
        "what was stored should load as the settings the application is running with"
    );
}

/// Work that writes the settings while the page is open is not the page's to undo.
///
/// An avatar is generated in the background and a workspace layout is persisted on a
/// delay, so the settings of the moment a change is made are not the settings the page was
/// opened with: what the user changes is applied to the settings of that moment rather than
/// published as the snapshot the page started from.
#[gpui_kit::test]
fn a_background_write_should_survive_the_next_settings_write(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    write_while_open(cx);
    settle(cx);

    edit_settings(&panel, |config| config.appearance = Appearance::Dark, cx);

    let published = stored(cx);
    assert_eq!(
        published.appearance,
        Appearance::Dark,
        "the change the user chose should be the one that is stored"
    );
    assert_eq!(
        published.avatar.as_deref(),
        Some("<svg>generated-while-open</svg>"),
        "a change must not roll back the avatar the background wrote"
    );
    assert_eq!(
        published
            .account
            .as_ref()
            .map(|account| account.user_id.as_str()),
        Some("22222222-2222-4222-8222-222222222222"),
        "a change must not roll back the account another subsystem signed in"
    );
    assert_eq!(
        published.workspaces.active.as_deref(),
        Some("written-while-open"),
        "a change must not roll back a workspace layout persisted while it was open"
    );
}

/// The sidebar state is stored beside the settings the page owns, so a change made while the
/// state moved has to write the state of that moment: replaying the snapshot the page was
/// opened with would put the sidebar back where it was when the page opened.
#[gpui_kit::test]
fn a_background_sidebar_state_should_survive_the_next_settings_write(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    write_while_open(cx);
    settle(cx);

    edit_settings(&panel, |config| config.appearance = Appearance::Dark, cx);

    assert_eq!(
        stored(cx).ui.sidebar,
        SidebarMode::IconOnly,
        "a change must not roll back the sidebar state written while it was open"
    );
    assert_eq!(
        stored(cx).appearance,
        Appearance::Dark,
        "the change the user chose should be the one that is stored"
    );
}

/// A reason a write failed is not lost when the page is left before the frame that reports it:
/// whoever leaves the page asks for it, and it is handed over once.
#[gpui_kit::test]
fn a_write_failure_waiting_to_be_reported_should_be_handed_to_whoever_leaves(
    cx: &mut TestAppContext,
) {
    let (manager, _blocking, _directory) = manager_behind_a_file(cx);
    let (panel, cx) = settings_over(manager, Language::En, cx);

    // One turn's worth of state: the write fails, and nothing has drawn the page that would
    // report it before the leave asks for the reason.
    let reason = panel.update(cx, |panel, cx| {
        panel.session_for_test().update(cx, |session, cx| {
            let _ = session.write(
                SettingsChange::edit(|config| config.appearance = Appearance::Dark),
                cx,
            );
        });
        panel.take_write_failure(cx)
    });

    assert!(
        reason.is_some_and(|reason| !reason.is_empty()),
        "the reason the write failed should be handed to whoever leaves the page"
    );
    assert_eq!(
        stored(cx).appearance,
        Appearance::System,
        "settings that could not be stored must not be published as if they had been"
    );
    assert!(
        panel
            .update(cx, |panel, cx| panel.take_write_failure(cx))
            .is_none(),
        "one failure should be reported once"
    );
    click(cx, "SETTINGS_ENTRY_appearance");
    assert_eq!(
        current_page(&panel, cx),
        "appearance",
        "a failed write must not leave the page unable to move"
    );
}

/// A choice that could not be stored keeps the old value, reports why, and leaves the page
/// open.
#[gpui_kit::test]
fn a_failed_select_write_should_keep_the_old_value_and_report(cx: &mut TestAppContext) {
    let (manager, _blocking, _directory) = manager_behind_a_file(cx);
    let (panel, cx) = settings_over(manager, Language::ZhCn, cx);

    edit_settings(&panel, |config| config.language = Language::En, cx);

    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "a write that could not be stored should tell the user"
    );
    assert_eq!(
        stored(cx).language,
        Language::ZhCn,
        "settings that could not be stored must not be published as if they had been"
    );
    assert_eq!(
        cx.read(|cx| *cx.global::<LanguageCode>()),
        LanguageCode::ZhCn,
        "an interface the settings did not move should stay where it was"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "the settings should stay open so the user can try again"
    );
    let shown = panel.read_with(cx, |panel, cx| {
        panel.session_for_test().read(cx).config(cx).language
    });
    assert_eq!(
        shown,
        Language::ZhCn,
        "the control should be drawn from the settings that are still stored"
    );
}

/// A value a setter refuses is the user's choice kept as the old value: it is not a failure
/// and there is nothing to report.
#[gpui_kit::test]
fn a_setter_that_rejects_the_value_should_keep_the_old_value_without_a_report(
    cx: &mut TestAppContext,
) {
    let mut config = Config::default();
    config.venues.remove(&VenueId::Okx);
    let (panel, cx) = settings(config, cx);
    let before = stored(cx);

    edit_settings(
        &panel,
        |config| {
            config.set_okx_credentials(
                &VenueId::Okx,
                Some(OkxCredentials {
                    api_key: SmolStr::new_static("typed-key"),
                    secret_key: SmolStr::new_static("typed-secret"),
                    passphrase: SmolStr::new_static("typed-passphrase"),
                }),
            );
        },
        cx,
    );

    assert_eq!(
        stored(cx),
        before,
        "a refused value must leave the settings exactly as they were"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_none(),
        "a value the settings refused is not a failure to report"
    );
}

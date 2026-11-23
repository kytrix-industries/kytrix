//! The tests of the appearance page: the theme and the language it switches.

use app_config::settings::{Appearance, Language};
use base_ui::theme::appearance::Appearance as ThemeAppearance;

use crate::settings_tests::test_support::Harness;

/// The page the settings open on is the appearance page, and its two settings are drawn.
#[test]
fn the_settings_should_open_on_the_appearance_page() {
    let mut harness = Harness::new();
    harness.settle(2);

    assert_eq!(harness.page().current_page(), "appearance");
    assert!(
        harness.field("appearance").is_some(),
        "the appearance setting should be drawn"
    );
    assert!(
        harness.field("language").is_some(),
        "the language setting should be drawn"
    );
}

/// Switching the appearance in the settings switches the theme.
#[test]
fn a_stored_dark_appearance_should_be_the_applied_theme() {
    let mut harness = Harness::new();
    harness.settle(2);
    assert_eq!(
        harness.theme().current(harness.app()),
        ThemeAppearance::Light
    );

    harness.update_config(|config| config.appearance = Appearance::Dark);
    harness.settle(1);

    assert_eq!(
        harness.theme().current(harness.app()),
        ThemeAppearance::Dark
    );
}

/// Switching the language in the settings redraws the page's static text in the new language.
#[test]
fn a_stored_language_should_redraw_the_page_in_it() {
    let mut harness = Harness::new();
    harness.settle(2);
    let english = harness.field_label("appearance");
    assert_eq!(english, "Mode");

    harness.update_config(|config| config.language = Language::ZhCn);
    harness.settle(2);

    assert_eq!(harness.field_label("appearance"), "模式");
}

/// A settings page whose language was written through the session follows it on the next frame.
#[test]
fn a_language_written_through_the_session_should_be_drawn() {
    let mut harness = Harness::new();
    harness.settle(2);

    harness.update_config(|config| config.language = Language::ZhCn);
    harness.settle(2);

    assert_eq!(harness.page().query(), "");
    assert!(
        harness
            .debug_ids()
            .iter()
            .any(|id| id.starts_with("SETTINGS_FIELD_")),
        "the page should still be drawn after the language switch"
    );
}

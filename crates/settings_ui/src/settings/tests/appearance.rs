//! The tests of the page's appearance and language: what is stored is what is drawn.

use app_config::settings::{Appearance, Config, Language};
use base_ui::style::palette::Appearance as ThemeAppearance;
use gpui_kit::{SharedString, TestAppContext, WindowAppearance};
use i18n::language::codes::LanguageCode;

use super::test_support::*;

/// A value the user changes reaches the settings at once: there is nothing to confirm.
#[gpui_kit::test]
fn a_field_change_should_reach_the_settings_at_once(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);

    write_field(&panel, "appearance", "appearance", "Dark", cx);

    assert_eq!(
        stored(cx).appearance,
        Appearance::Dark,
        "the change should be the settings of the moment it was made"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "the page has no commit that closes it, so a change leaves it open"
    );
}

/// The theme is what the settings name, as soon as they name it.
///
/// A control writes the settings rather than a copy of them, so the appearance the interface
/// is drawn in follows the stored preference at once.
#[gpui_kit::test]
fn the_theme_should_follow_the_appearance_at_once(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);

    write_field(&panel, "appearance", "appearance", "Dark", cx);

    assert_eq!(
        drawn_appearance(cx),
        ThemeAppearance::Dark,
        "the appearance the user chose should be the one the application is drawn in"
    );
}

/// The theme is drawn from the appearance the settings name, and a preference that names an
/// appearance is not the system's to answer.
///
/// The precedence itself — every preference against every appearance a system reports, the
/// pinned cases included — is a table in [`crate::appearance`], where the system appearance is
/// an argument and so can be put to it. What only a whole session can show is that the resolved
/// appearance reaches the theme: a chosen `Dark` draws the dark appearance, and handing the
/// preference back to the system draws the appearance the system's own appearance resolves
/// to — which the window reports itself, so the test asks the window rather than assuming one.
#[gpui_kit::test]
fn an_appearance_should_draw_the_appearance_it_resolves_to(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);

    write_field(&panel, "appearance", "appearance", "Dark", cx);

    assert_eq!(
        drawn_appearance(cx),
        ThemeAppearance::Dark,
        "the appearance the user chose is the one the application is drawn in"
    );

    let reported = cx.update(|window, _| window.appearance());
    write_field(&panel, "appearance", "appearance", "System", cx);

    assert_eq!(
        stored(cx).appearance,
        Appearance::System,
        "the settings should have moved the preference back to the system"
    );
    assert_eq!(
        drawn_appearance(cx),
        match reported {
            WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeAppearance::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeAppearance::Dark,
        },
        "following the system again means being drawn in the appearance the system reports"
    );
}

/// The language switches at once, the sidebar is labelled in it, and the page the user was on
/// is still reachable — or the page moves to one that is.
#[gpui_kit::test]
fn the_language_should_switch_at_once_and_the_page_should_stay_reachable(cx: &mut TestAppContext) {
    let (panel, cx) = settings_in(Config::default(), Language::ZhCn, cx);

    assert!(
        labels(&panel, cx).contains(&SharedString::new_static("外观")),
        "the sidebar should open in the language the window opened in"
    );

    write_field(&panel, "appearance", "language", "en", cx);

    assert_eq!(
        cx.read(|cx| *cx.global::<LanguageCode>()),
        LanguageCode::En,
        "choosing the language should switch the interface to it at once"
    );
    assert_eq!(
        stored(cx).language,
        Language::En,
        "choosing the language should store it at once"
    );
    let translated = labels(&panel, cx);
    assert!(
        translated.contains(&SharedString::new_static("Appearance")),
        "the sidebar should be labelled in the language that was chosen: {translated:?}"
    );
    assert_eq!(
        current_page(&panel, cx),
        "appearance",
        "the page that was drawn should still be the page the new language reaches"
    );
    assert!(
        cx.debug_bounds("SETTINGS_PAGE").is_some(),
        "switching the language leaves the settings page open"
    );
}

use std::io;

use app_config::settings::{Appearance, Language};
use app_config_provider::store::ConfigPersistenceError;
use i18n::t_smol;
use ui_core::event::{Action, KeyCode};
use ui_kit::theme::appearance::Appearance as ThemeAppearance;

use crate::{session::SettingsWriteError, settings_tests::test_support::Harness};

fn appearance_key(key: &str) -> String {
    format!("{key}.appearance.appearance")
}

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

#[test]
fn choosing_dark_in_the_appearance_select_should_store_and_apply_it() {
    let mut harness = Harness::new();
    harness.settle(3);

    choose(&mut harness, &appearance_key("settings.test"), 1);

    assert_eq!(
        harness.config().appearance,
        Appearance::Dark,
        "the choice should be stored as the appearance preference"
    );
    assert_eq!(
        harness.theme().current(harness.app()),
        ThemeAppearance::Dark,
        "the theme should be the appearance the settings name"
    );
    assert_eq!(
        harness.writes().len(),
        1,
        "one choice stores one configuration"
    );
}

#[test]
fn choosing_a_language_should_store_it_and_redraw_the_page() {
    let mut harness = Harness::new();
    harness.settle(3);
    assert_eq!(harness.field_label("appearance"), "Mode");

    choose(&mut harness, "settings.test.appearance.language", 0);

    assert_eq!(harness.config().language, Language::ZhCn);
    assert_eq!(
        harness.field_label("appearance"),
        "模式",
        "the rows should be drawn in the language the settings name"
    );
}

#[test]
fn a_write_that_cannot_be_stored_should_be_reported_over_the_page() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.fail_next_write(SettingsWriteError::new(
        ConfigPersistenceError::CreateDirectory {
            path: std::path::PathBuf::from("unwritable"),
            source: io::Error::other("the directory cannot be created"),
        },
    ));
    let title = t_smol!(
        harness.services().translate,
        "settings_ui.save_failed.title"
    );

    choose(&mut harness, &appearance_key("settings.test"), 1);
    harness.settle(3);

    assert_eq!(
        harness.config().appearance,
        Appearance::System,
        "a write that could not be stored must leave the settings alone"
    );
    assert!(
        harness.has_text(harness.root(), title.as_str()),
        "the page should report why the write failed"
    );
    assert!(
        !harness.has_write_failure(),
        "the reason should be taken out of the session once it was reported"
    );
}

#[test]
fn a_second_page_should_not_share_the_first_pages_controls() {
    let mut harness = Harness::new();
    harness.settle(3);
    harness.mount_second("settings.other");
    harness.settle(3);

    let first = Action::Custom(format!("{}.trigger", appearance_key("settings.test")).into());
    let second = Action::Custom(format!("{}.trigger", appearance_key("settings.other")).into());
    assert!(
        harness.app().has_action_handler(&first),
        "the first page's appearance control should be wired"
    );
    assert!(
        harness.app().has_action_handler(&second),
        "the second page's appearance control should be wired"
    );

    let mut released = harness.take_second().expect("the second page was mounted");
    released.release(harness.app_mut());
    harness.drain();

    assert!(
        harness.app().has_action_handler(&first),
        "releasing the second page must not unwire the first page's control"
    );
    assert!(
        !harness.app().has_action_handler(&second),
        "releasing the second page should unwire its own control"
    );
}

#[test]
fn releasing_the_page_should_take_its_nodes_and_actions_away() {
    let mut harness = Harness::new();
    harness.settle(3);
    let mut page = {
        let host = harness.host().clone();
        let root = harness.root();
        let window = harness.window();
        let app = harness.app_mut();
        crate::settings::SettingsPage::mount(host, "settings.released", app, window, root)
            .expect("a settings page mounts under the harness window")
    };
    harness.drain();
    let window = harness.window();
    let drawn = harness
        .app()
        .window(window)
        .expect("the window exists")
        .tree()
        .len();
    assert!(
        harness.find_all("SETTINGS_PAGE").len() >= 2,
        "both settings pages should be drawn before one is released"
    );

    let released_action =
        Action::Custom(format!("{}.trigger", appearance_key("settings.released")).into());
    assert!(
        harness.app().has_action_handler(&released_action),
        "the page's control should be wired before it is released"
    );

    page.release(harness.app_mut());
    harness.drain();

    assert!(
        harness.app().window(window).is_some(),
        "releasing a page must not close its window"
    );
    assert!(
        harness
            .app()
            .window(window)
            .expect("the window exists")
            .tree()
            .len()
            < drawn,
        "releasing the page should remove the nodes it built"
    );
    assert!(
        !harness.app().has_action_handler(&released_action),
        "releasing the page should unwire the controls it built"
    );
    assert!(
        harness.find("SETTINGS_PAGE").is_some(),
        "the harness's own page should still be drawn"
    );
}

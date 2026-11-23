use std::io;

use app_config::settings::Appearance;
use app_config_provider::store::ConfigPersistenceError;
use i18n::t_smol;

use crate::{
    session::{SettingsChange, SettingsWriteError},
    settings_tests::test_support::Harness,
};

fn store_failure(reason: &'static str) -> SettingsWriteError {
    SettingsWriteError::new(ConfigPersistenceError::CreateDirectory {
        path: std::path::PathBuf::from("unwritable"),
        source: io::Error::other(reason),
    })
}

#[test]
fn a_write_that_could_not_be_stored_should_be_stored_when_it_is_tried_again() {
    let mut harness = Harness::new();
    let before = harness.config();
    harness.fail_next_write(store_failure("the directory cannot be created"));

    harness
        .write(SettingsChange::edit(|config| {
            config.appearance = Appearance::Dark
        }))
        .expect_err("the store refused the first write");
    assert_eq!(
        harness.config(),
        before,
        "the refused write leaves the settings as they were"
    );

    harness
        .write(SettingsChange::edit(|config| {
            config.appearance = Appearance::Dark
        }))
        .expect("the retry stores the change");
    assert_eq!(harness.config().appearance, Appearance::Dark);
}

#[test]
fn a_write_failure_waiting_to_be_reported_should_be_handed_to_whoever_leaves() {
    let mut harness = Harness::new();
    harness.fail_next_write(store_failure("the directory cannot be created"));
    harness
        .write(SettingsChange::edit(|config| {
            config.appearance = Appearance::Dark
        }))
        .expect_err("the store refused the write");

    let reason = harness
        .page_mut()
        .take_write_failure()
        .expect("the page hands the reason to the shell that leaves it");
    assert!(
        reason.contains("the directory cannot be created"),
        "the reason the page hands over is the store's own: {reason}"
    );
    assert!(
        harness.page_mut().take_write_failure().is_none(),
        "one failure is handed over once"
    );
    assert!(
        !harness.has_write_failure(),
        "the handover takes the failure out of the session"
    );
}

#[test]
fn the_failure_alert_should_name_the_reason_in_both_languages() {
    let mut harness = Harness::new();
    harness.settle(2);
    let english = t_smol!(
        harness.services().translate,
        "settings_ui.save_failed.title"
    );
    harness.fail_next_write(store_failure("the directory cannot be created"));
    harness
        .write(SettingsChange::edit(|config| {
            config.appearance = Appearance::Dark
        }))
        .expect_err("the store refused the write");
    harness.settle(3);
    assert!(
        harness.has_text(harness.root(), english.as_str()),
        "the report should be the English sentence"
    );

    let mut chinese = Harness::new();
    chinese.update_config(|config| config.language = app_config::settings::Language::ZhCn);
    chinese.settle(2);
    let localized = t_smol!(
        chinese.services().translate,
        "settings_ui.save_failed.title"
    );
    assert_ne!(localized, english, "the two languages differ");
    chinese.fail_next_write(store_failure("the directory cannot be created"));
    chinese
        .write(SettingsChange::edit(|config| {
            config.appearance = Appearance::Dark
        }))
        .expect_err("the store refused the write");
    chinese.settle(3);
    assert!(
        chinese.has_text(chinese.root(), localized.as_str()),
        "the report should be the Chinese sentence"
    );
}

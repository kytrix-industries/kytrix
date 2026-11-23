use std::{
    path::Path,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use app_config::{
    settings::{Config, Language},
    translation::TranslationModel,
};
use i18n::language::codes::LanguageCode;
use smol_str::SmolStr;
use tempfile::TempDir;

use crate::{
    pages::translation::table::size_text,
    settings_tests::test_support::{
        FakeDownloads, Harness, fake_downloads, send_event, translate_in,
    },
    translation_downloads::{DownloadState, TranslationDownloads},
};

const KEY: &str = "settings.test";
const PAGE: &str = "ai.translation";

fn config_in(directory: &Path) -> Config {
    let mut config = Config::default();
    config.language = Language::En;
    config.translation.model_directory = Some(directory.to_path_buf());
    config
}

fn page(config: Config) -> (Harness, Rc<TranslationDownloads>, FakeDownloads) {
    let mut harness = Harness::with_config(config);
    let (downloads, fake) = fake_downloads(&mut harness, Arc::new(|_: &Path| Ok(())));
    harness.settle(1);
    harness.select_page("ai.translation");
    harness.settle(2);

    (harness, downloads, fake)
}

fn page_prepared(
    config: Config,
    prepare: impl FnOnce(&TranslationDownloads, &mut Harness),
) -> (Harness, Rc<TranslationDownloads>, FakeDownloads) {
    let mut harness = Harness::with_config(config);
    let (downloads, fake) = fake_downloads(&mut harness, Arc::new(|_: &Path| Ok(())));
    prepare(&downloads, &mut harness);
    harness.settle(1);
    harness.select_page("ai.translation");
    harness.settle(2);

    (harness, downloads, fake)
}
fn model_key() -> String {
    format!("{KEY}.{PAGE}.model")
}

fn open_model_select(harness: &mut Harness) {
    harness.click_node("SETTINGS_AI_MODEL_SELECT");
    harness.settle(1);
}

fn model_panel_text(harness: &Harness) -> String {
    let panel = harness
        .find(&format!("{}:panel:surface", model_key()))
        .expect("the model select should open its panel");
    harness.text_of(panel)
}

fn choose_model(harness: &mut Harness, index: usize) {
    harness.click_node(&format!("{}:panel:list:hit:{index}", model_key()));
    harness.settle(1);
}

fn spin(harness: &mut Harness, what: &str, mut done: impl FnMut(&Harness) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(harness) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        harness.settle(1);
        std::thread::yield_now();
    }
}

fn commit_directory(harness: &mut Harness, text: &str) {
    harness.click_node("SETTINGS_AI_DIRECTORY_INPUT");
    press_key(
        harness,
        ui_core::event::KeyCode::KeyA,
        ui_core::event::Modifiers::CONTROL,
    );
    send_event(
        harness,
        ui_core::event::Event::Input(ui_core::event::InputEvent::Text {
            text: SmolStr::new(text),
            timestamp: Instant::now(),
        }),
    );
    press_key(
        harness,
        ui_core::event::KeyCode::Enter,
        ui_core::event::Modifiers::empty(),
    );
    harness.settle(2);
}

fn press_key(
    harness: &mut Harness,
    code: ui_core::event::KeyCode,
    modifiers: ui_core::event::Modifiers,
) {
    send_event(
        harness,
        ui_core::event::Event::Input(ui_core::event::InputEvent::Key {
            code,
            state: ui_core::event::KeyState::Down,
            repeat: false,
            modifiers,
            timestamp: Instant::now(),
        }),
    );
}
#[test]
fn the_model_select_should_list_only_downloaded_models() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, downloads, _fake) = page(config_in(directory.path()));
    downloads
        .force_downloaded_for_test(TranslationModel::HyMt27BQ8, directory.path().to_path_buf());
    downloads.force_downloaded_for_test(
        TranslationModel::HyMt218BQ6K,
        directory.path().to_path_buf(),
    );
    harness.settle(1);

    open_model_select(&mut harness);

    let text = model_panel_text(&harness);
    assert!(
        text.contains(TranslationModel::HyMt218BQ6K.display_name()),
        "the downloaded model is offered: {text:?}"
    );
    assert!(
        text.contains(TranslationModel::HyMt27BQ8.display_name()),
        "the downloaded model is offered: {text:?}"
    );
    assert!(
        harness
            .find(&format!("{}:panel:list:hit:2", model_key()))
            .is_none(),
        "only the downloaded models are offered"
    );
    let first = harness
        .bounds(
            harness
                .find(&format!("{}:panel:list:hit:0", model_key()))
                .expect("the first offered model"),
        )
        .expect("the first offered model is laid out");
    let second = harness
        .bounds(
            harness
                .find(&format!("{}:panel:list:hit:1", model_key()))
                .expect("the second offered model"),
        )
        .expect("the second offered model is laid out");
    assert!(
        first.origin.y < second.origin.y,
        "the offered models are in page order"
    );
}

#[test]
fn choosing_a_model_should_store_the_selection_at_once() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = TranslationModel::HyMt218BQ4Km;
    let (mut harness, downloads, _fake) = page(config_in(directory.path()));
    downloads.force_downloaded_for_test(model, directory.path().to_path_buf());
    harness.settle(1);

    open_model_select(&mut harness);
    choose_model(&mut harness, 0);

    assert_eq!(
        harness.config().translation.selected_model,
        Some(model),
        "choosing a model stores it at once"
    );
}

#[test]
fn a_model_being_deleted_should_not_be_offered() {
    let directory = TempDir::new().expect("the test directory builds");
    let removed = TranslationModel::HyMt218BQ4Km;
    let kept = TranslationModel::HyMt27BQ8;
    let (mut harness, downloads, _fake) = page(config_in(directory.path()));
    downloads.force_downloaded_for_test(removed, directory.path().to_path_buf());
    downloads.force_downloaded_for_test(kept, directory.path().to_path_buf());
    downloads.set_state_for_test(
        removed,
        directory.path().to_path_buf(),
        DownloadState::Deleting,
    );
    harness.settle(2);

    open_model_select(&mut harness);

    let text = model_panel_text(&harness);
    assert!(
        !text.contains(removed.display_name()),
        "a model being removed must not be offered: {text:?}"
    );
    assert!(
        text.contains(kept.display_name()),
        "a downloaded model that is staying is offered: {text:?}"
    );
}

#[test]
fn the_model_select_should_show_the_model_in_use() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = TranslationModel::HyMt27BQ8;
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(model);
    let (harness, _downloads, _fake) = page_prepared(config, |downloads, _| {
        downloads.force_downloaded_for_test(model, directory.path().to_path_buf());
    });

    let trigger = harness
        .find("SETTINGS_AI_MODEL_SELECT")
        .expect("the model select is drawn");
    assert!(
        harness.text_of(trigger).contains(model.display_name()),
        "the trigger names the model in use"
    );
}

#[test]
fn a_selection_the_directory_does_not_hold_should_be_cleared() {
    let directory = TempDir::new().expect("the test directory builds");
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
    let (harness, _downloads, _fake) = page(config);

    assert_eq!(
        harness.config().translation.selected_model,
        None,
        "a model the directory does not hold must not stay selected"
    );
    assert_eq!(
        harness.config().translation.model_directory,
        Some(directory.path().to_path_buf()),
        "the directory itself is already stored and is not replaced"
    );
}

#[test]
fn a_selection_that_is_being_removed_should_be_cleared() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = TranslationModel::HyMt27BQ8;
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(model);
    let (harness, _downloads, _fake) = page_prepared(config, |downloads, _| {
        downloads.force_downloaded_for_test(model, directory.path().to_path_buf());
        downloads.set_state_for_test(
            model,
            directory.path().to_path_buf(),
            DownloadState::Deleting,
        );
    });

    assert_eq!(
        harness.config().translation.selected_model,
        None,
        "a model being removed must not stay selected"
    );
}

#[test]
fn the_page_should_not_write_when_the_settings_are_consistent() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = TranslationModel::HyMt218BQ4Km;
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(model);
    let (mut harness, _downloads, _fake) = page_prepared(config, |downloads, _| {
        downloads.force_downloaded_for_test(model, directory.path().to_path_buf());
    });
    let writes = harness.writes().len();

    harness.settle(3);

    assert_eq!(
        harness.writes().len(),
        writes,
        "a steady page writes nothing"
    );
}

#[test]
fn changing_the_directory_should_clear_the_selection_it_does_not_hold() {
    let first = TempDir::new().expect("the first directory builds");
    let second = TempDir::new().expect("the second directory builds");
    let model = TranslationModel::HyMt218BQ4Km;
    let mut config = config_in(first.path());
    config.translation.selected_model = Some(model);
    let (mut harness, downloads, _fake) = page(config);
    downloads.force_downloaded_for_test(model, first.path().to_path_buf());
    harness.settle(1);

    commit_directory(&mut harness, second.path().to_string_lossy().as_ref());

    let stored = harness.config();
    assert_eq!(
        stored.translation.model_directory,
        Some(second.path().to_path_buf())
    );
    assert_eq!(
        stored.translation.selected_model, None,
        "the model is not in the chosen directory, so the choice goes"
    );
}

#[test]
fn a_background_selection_should_be_cleared_by_the_directory_change() {
    let stored_directory = TempDir::new().expect("the stored directory builds");
    let chosen_directory = TempDir::new().expect("the chosen directory builds");
    let selected = TranslationModel::HyMt27BQ8;
    let (mut harness, downloads, _fake) = page(config_in(stored_directory.path()));
    downloads.force_downloaded_for_test(selected, stored_directory.path().to_path_buf());
    harness.update_config(|config| config.translation.selected_model = Some(selected));
    harness.settle(2);

    commit_directory(
        &mut harness,
        chosen_directory.path().to_string_lossy().as_ref(),
    );

    let stored = harness.config();
    assert_eq!(
        stored.translation.model_directory,
        Some(chosen_directory.path().to_path_buf()),
        "the directory the user chose should be stored"
    );
    assert_eq!(
        stored.translation.selected_model, None,
        "a selection the new directory does not hold is cleared with it"
    );
}

#[test]
fn a_failed_model_selection_should_keep_the_stored_selection() {
    let directory = TempDir::new().expect("the test directory builds");
    let selected = TranslationModel::HyMt27BQ8;
    let other = TranslationModel::HyMt218BQ4Km;
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(selected);
    let (mut harness, _downloads, _fake) = page_prepared(config, |downloads, _| {
        downloads.force_downloaded_for_test(selected, directory.path().to_path_buf());
        downloads.force_downloaded_for_test(other, directory.path().to_path_buf());
    });
    harness.fail_next_write(crate::session::SettingsWriteError::new(
        app_config_provider::store::ConfigPersistenceError::CreateDirectory {
            path: std::path::PathBuf::from("unwritable"),
            source: std::io::Error::other("the directory cannot be created"),
        },
    ));

    open_model_select(&mut harness);
    choose_model(&mut harness, 0);

    assert_eq!(
        harness.config().translation.selected_model,
        Some(selected),
        "a selection that could not be stored leaves the stored selection as it was"
    );
    assert!(
        harness.find("settings.test.save-failed").is_some(),
        "a selection that could not be stored should report why"
    );
}

#[test]
fn deleting_a_selected_model_should_clear_the_selection() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = TranslationModel::HyMt218BQ4Km;
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"stub").expect("the fixture file should be written");
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(model);
    let (mut harness, downloads, _fake) = page(config);
    downloads.force_downloaded_for_test(model, directory.path().to_path_buf());
    harness.settle(2);

    harness.click_node("SETTINGS_AI_MODEL_DELETE_0");
    assert_eq!(
        harness.config().translation.selected_model,
        None,
        "the stored selection is cleared when the removal starts, before the file goes"
    );

    spin(&mut harness, "the model file to be deleted", |harness| {
        !path.exists() && harness.config().translation.selected_model.is_none()
    });
    assert!(!path.exists(), "the model file is deleted");
}

#[test]
fn deleting_another_model_should_keep_the_selected_model() {
    let directory = TempDir::new().expect("the test directory builds");
    let selected = TranslationModel::HyMt218BQ4Km;
    let other = TranslationModel::HyMt27BQ8;
    std::fs::write(directory.path().join(selected.file_name()), b"stub")
        .expect("the fixture file should be written");
    std::fs::write(directory.path().join(other.file_name()), b"stub")
        .expect("the fixture file should be written");
    let mut config = config_in(directory.path());
    config.translation.selected_model = Some(selected);
    let (mut harness, _downloads, _fake) = page_prepared(config, |downloads, _| {
        downloads.force_downloaded_for_test(selected, directory.path().to_path_buf());
        downloads.force_downloaded_for_test(other, directory.path().to_path_buf());
    });

    harness.click_node("SETTINGS_AI_MODEL_DELETE_7");
    let other_path = directory.path().join(other.file_name());
    spin(&mut harness, "the other model's file to be deleted", |_| {
        !other_path.exists()
    });

    assert_eq!(
        harness.config().translation.selected_model,
        Some(selected),
        "deleting another model must not clear the selected one"
    );
}

#[test]
fn a_completed_download_should_select_the_stored_model() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = TranslationModel::HyMt218BQ8;
    let (mut harness, downloads, fake) = page(config_in(directory.path()));
    harness.click_node("SETTINGS_AI_MODEL_DOWNLOAD_2");
    let started = fake
        .added()
        .expect("the download is added to the fake manager");
    downloads.force_downloaded_for_test(model, directory.path().to_path_buf());
    fake.push(
        harness.app_mut(),
        crate::settings_tests::test_support::download_task(
            started,
            directory.path().join(model.file_name()),
            download_provider::task::DownloadStatus::Completed {
                downloaded: 100,
                total: 100,
            },
        ),
    );
    harness.settle(2);

    assert_eq!(
        harness.config().translation.selected_model,
        Some(model),
        "the finished download is what the page now uses"
    );
}

#[test]
fn the_table_headers_should_be_named_in_the_interface_language() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, _downloads, _fake) = page(config_in(directory.path()));
    harness.settle(1);

    let name = harness
        .find("SETTINGS_AI_COLUMN_NAME")
        .expect("the model-name header is drawn");
    assert_eq!(harness.node_text(name), "Model name");
    let size = harness
        .find("SETTINGS_AI_COLUMN_SIZE")
        .expect("the size header is drawn");
    assert_eq!(harness.node_text(size), "Size");
    let status = harness
        .find("SETTINGS_AI_COLUMN_STATUS")
        .expect("the status header is drawn");
    assert_eq!(harness.node_text(status), "Status");
}

#[test]
fn the_table_headers_should_follow_the_language() {
    let directory = TempDir::new().expect("the test directory builds");
    let mut config = config_in(directory.path());
    config.language = Language::ZhCn;
    let mut harness = Harness::build(config, translate_in(LanguageCode::ZhCn));
    harness.settle(2);
    harness.select_page("ai.translation");
    harness.settle(2);

    let size = harness
        .find("SETTINGS_AI_COLUMN_SIZE")
        .expect("the size header is drawn");
    assert_eq!(harness.node_text(size), "大小");
}

#[test]
fn a_model_size_should_be_drawn_in_the_unit_that_keeps_it_short() {
    for (model, expected) in [
        (TranslationModel::HyMt218BQ4Km, "1.06 GB"),
        (TranslationModel::HyMt218B2Bit, "572.71 MB"),
        (TranslationModel::HyMt27BQ8, "7.43 GB"),
    ] {
        assert_eq!(
            size_text(model.size_bytes()).as_str(),
            expected,
            "{}",
            model.display_name()
        );
    }
    assert_eq!(size_text(1024 * 1024 * 1024).as_str(), "1024.00 MB");
    assert_eq!(size_text(512 * 1024 * 1024).as_str(), "512.00 MB");
}

use std::{path::Path, rc::Rc, sync::Arc, time::Instant};

use app_config::{
    settings::{Config, Language},
    translation::{DownloadSite, TranslationModel},
};
use base_ui::kit::icon::IconKind;
use download_provider::error::DownloadFailure;
use i18n::{language::codes::LanguageCode, t_smol};
use smol_str::SmolStr;
use tempfile::TempDir;
use ui_core::event::{Event, InputEvent, KeyCode};

use crate::{
    pages::translation::table::{RowAction, size_text, speed_text, whole_percent},
    settings_tests::test_support::{
        FakeDownloads, Harness, download_task, fake_downloads, release_page, send_event,
        translate_in,
    },
    translation_downloads::{DeleteFailure, DownloadState, TranslationDownloads, UnloadFailure},
};

const KEY: &str = "settings.test.ai.translation";

fn config_in(directory: &Path) -> Config {
    let mut config = Config::default();
    config.language = Language::En;
    config.translation.model_directory = Some(directory.to_path_buf());
    config
}

fn page_prepared(
    config: Config,
    prepare: impl FnOnce(&TranslationDownloads),
) -> (Harness, Rc<TranslationDownloads>, FakeDownloads) {
    let mut harness = Harness::with_config(config);
    let (downloads, fake) = fake_downloads(&mut harness, Arc::new(|_: &Path| Ok(())));
    prepare(&downloads);
    harness.settle(1);
    harness.select_page("ai.translation");
    harness.settle(2);

    (harness, downloads, fake)
}

fn page(config: Config) -> (Harness, Rc<TranslationDownloads>, FakeDownloads) {
    page_prepared(config, |_| {})
}

fn page_with(directory: &TempDir) -> (Harness, Rc<TranslationDownloads>, FakeDownloads) {
    page(config_in(directory.path()))
}

fn press_key(harness: &mut Harness, code: KeyCode, modifiers: ui_core::event::Modifiers) {
    send_event(
        harness,
        Event::Input(InputEvent::Key {
            code,
            state: ui_core::event::KeyState::Down,
            repeat: false,
            modifiers,
            timestamp: Instant::now(),
        }),
    );
}

fn replace_directory(harness: &mut Harness, text: &str) {
    harness.click_node("SETTINGS_AI_DIRECTORY_INPUT");
    press_key(harness, KeyCode::KeyA, ui_core::event::Modifiers::CONTROL);
    send_event(
        harness,
        Event::Input(InputEvent::Text {
            text: SmolStr::new(text),
            timestamp: Instant::now(),
        }),
    );
    press_key(harness, KeyCode::Enter, ui_core::event::Modifiers::empty());
    harness.settle(2);
}

fn action_drawn(harness: &Harness, action: &str, index: usize) -> bool {
    harness
        .find(&format!("SETTINGS_AI_MODEL_{action}_{index}"))
        .is_some()
}

fn row_count(harness: &Harness) -> usize {
    (0..8)
        .filter(|index| {
            harness
                .find(&format!("SETTINGS_AI_MODEL_{index}"))
                .is_some()
        })
        .count()
}

#[test]
fn the_translation_page_should_draw_its_settings_and_models() {
    let directory = TempDir::new().expect("the test directory builds");
    let (harness, _downloads, _fake) = page_with(&directory);

    for selector in [
        "SETTINGS_AI_PAGE",
        "SETTINGS_CARD_ai.translation",
        "SETTINGS_FIELD_ai.directory",
        "SETTINGS_AI_DIRECTORY_INPUT",
        "SETTINGS_FIELD_ai.download-site",
        "SETTINGS_AI_SITE_SELECT",
        "SETTINGS_FIELD_ai.model",
        "SETTINGS_AI_MODEL_SELECT",
        "SETTINGS_AI_MODELS",
        "SETTINGS_AI_MODEL_TABLE",
        "SETTINGS_AI_MODEL_0",
        "SETTINGS_AI_MODEL_7",
        "SETTINGS_AI_MODEL_DOWNLOAD_0",
        "SETTINGS_AI_MODEL_DOWNLOAD_7",
    ] {
        assert!(
            harness.find(selector).is_some(),
            "the page should draw {selector}"
        );
    }
    assert_eq!(row_count(&harness), 8, "the table draws every model");

    let directory_row = harness
        .bounds(harness.field("ai.directory").expect("the directory row"))
        .expect("the directory row is laid out");
    let site_row = harness
        .bounds(harness.field("ai.download-site").expect("the site row"))
        .expect("the site row is laid out");
    let model_row = harness
        .bounds(harness.field("ai.model").expect("the model row"))
        .expect("the model row is laid out");
    let models = harness
        .bounds(
            harness
                .find("SETTINGS_AI_MODELS")
                .expect("the models section"),
        )
        .expect("the models section is laid out");
    assert!(directory_row.origin.y < site_row.origin.y);
    assert!(site_row.origin.y < model_row.origin.y);
    assert!(
        model_row.origin.y < models.origin.y,
        "the table is what the model section holds"
    );

    let rows = (0..8)
        .map(|index| {
            harness
                .bounds(
                    harness
                        .find(&format!("SETTINGS_AI_MODEL_{index}"))
                        .expect("the model row"),
                )
                .expect("the model row is laid out")
        })
        .collect::<Vec<_>>();
    for (index, pair) in rows.windows(2).enumerate() {
        assert!(
            pair[0].origin.y < pair[1].origin.y,
            "the table draws the models in page order: row {index} is above row {}",
            index + 1
        );
    }
}

#[test]
fn a_page_without_downloads_should_offer_no_download() {
    let directory = TempDir::new().expect("the test directory builds");
    let mut harness = Harness::with_config(config_in(directory.path()));
    harness.settle(1);
    harness.select_page("ai.translation");
    harness.settle(2);

    assert_eq!(row_count(&harness), 8, "the models are still drawn");
    assert!(
        harness.find("SETTINGS_AI_MODEL_DOWNLOAD_0").is_some(),
        "the download action is drawn, disabled"
    );
    harness.click_node("SETTINGS_AI_MODEL_DOWNLOAD_0");
    assert!(
        harness.find("SETTINGS_AI_MODEL_PROGRESS_0").is_none(),
        "a page without downloads starts nothing"
    );
}
#[test]
fn a_stored_directory_should_be_shown_in_its_field() {
    let directory = TempDir::new().expect("the test directory builds");
    let (harness, _downloads, _fake) = page_with(&directory);

    let field = harness
        .find("SETTINGS_AI_DIRECTORY_INPUT")
        .expect("the directory field is drawn");
    let text = harness.text_of(field);
    assert!(
        text.contains(directory.path().to_string_lossy().as_ref()),
        "the field should show the stored directory: {text:?}"
    );
    let bounds = harness.bounds(field).expect("the field is laid out");
    let control = harness
        .bounds(harness.field("ai.directory").expect("the directory row"))
        .expect("the directory row is laid out");
    assert!(
        bounds.size.width <= control.size.width,
        "the field stays inside the row: {bounds:?} in {control:?}"
    );
    assert!(
        bounds.size.height < 40.0,
        "the field stays on one line: {bounds:?}"
    );
}

#[test]
fn a_committed_directory_should_be_stored() {
    let stored = TempDir::new().expect("the stored directory builds");
    let chosen = TempDir::new().expect("the chosen directory builds");
    let (mut harness, _downloads, _fake) = page_with(&stored);

    harness.click_node("SETTINGS_AI_DIRECTORY_INPUT");
    replace_directory(&mut harness, chosen.path().to_string_lossy().as_ref());

    assert_eq!(
        harness.config().translation.model_directory,
        Some(chosen.path().to_path_buf()),
        "the committed directory is stored"
    );
    let field = harness
        .find("SETTINGS_AI_DIRECTORY_INPUT")
        .expect("the directory field is drawn");
    assert!(
        harness
            .text_of(field)
            .contains(chosen.path().to_string_lossy().as_ref()),
        "the field shows the directory it stored"
    );
}

#[test]
fn committing_a_directory_should_repair_the_selection() {
    let stored = TempDir::new().expect("the stored directory builds");
    let holds_nothing = TempDir::new().expect("the empty directory builds");
    let holds_selected = TempDir::new().expect("the directory that holds the selection builds");
    let mut config = config_in(stored.path());
    config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
    let (mut harness, _downloads, _fake) = page_prepared(config, |downloads| {
        downloads
            .force_downloaded_for_test(TranslationModel::HyMt218BQ4Km, stored.path().to_path_buf());
        downloads.force_downloaded_for_test(
            TranslationModel::HyMt218BQ4Km,
            holds_selected.path().to_path_buf(),
        );
        downloads.force_downloaded_for_test(
            TranslationModel::HyMt218B125Bit,
            holds_selected.path().to_path_buf(),
        );
    });

    replace_directory(
        &mut harness,
        holds_selected.path().to_string_lossy().as_ref(),
    );
    assert_eq!(
        harness.config().translation.selected_model,
        Some(TranslationModel::HyMt218BQ4Km),
        "a selection the new directory holds is kept"
    );

    replace_directory(
        &mut harness,
        holds_nothing.path().to_string_lossy().as_ref(),
    );
    assert_eq!(
        harness.config().translation.selected_model,
        None,
        "a directory that holds no model clears the selection"
    );
}

#[test]
fn a_committed_directory_should_select_the_first_model_it_holds() {
    let stored = TempDir::new().expect("the stored directory builds");
    let chosen = TempDir::new().expect("the chosen directory builds");
    let (mut harness, downloads, _fake) = page(config_in(stored.path()));
    downloads
        .force_downloaded_for_test(TranslationModel::HyMt218BQ4Km, chosen.path().to_path_buf());
    downloads.force_downloaded_for_test(
        TranslationModel::HyMt218B125Bit,
        chosen.path().to_path_buf(),
    );
    harness.settle(1);

    replace_directory(&mut harness, chosen.path().to_string_lossy().as_ref());

    assert_eq!(
        harness.config().translation.selected_model,
        Some(TranslationModel::HyMt218B125Bit),
        "HY-MT2-1.8B-1.25Bit sorts first among the models the directory holds"
    );
}
#[test]
fn a_failed_directory_write_should_keep_the_stored_directory() {
    let stored = TempDir::new().expect("the stored directory builds");
    let chosen = TempDir::new().expect("the chosen directory builds");
    let (mut harness, _downloads, _fake) = page_with(&stored);
    harness.fail_next_write(crate::session::SettingsWriteError::new(
        app_config_provider::store::ConfigPersistenceError::CreateDirectory {
            path: std::path::PathBuf::from("unwritable"),
            source: std::io::Error::other("the directory cannot be created"),
        },
    ));

    replace_directory(&mut harness, chosen.path().to_string_lossy().as_ref());

    assert_eq!(
        harness.config().translation.model_directory,
        Some(stored.path().to_path_buf()),
        "a directory that could not be stored must not be published as if it had been"
    );
    assert!(
        harness.find("settings.test.save-failed").is_some(),
        "a write that could not be stored is reported"
    );
    let field = harness
        .find("SETTINGS_AI_DIRECTORY_INPUT")
        .expect("the directory field is drawn");
    assert!(
        harness
            .text_of(field)
            .contains(stored.path().to_string_lossy().as_ref()),
        "the field keeps showing the directory that is stored"
    );
}

#[test]
fn the_site_select_should_store_the_chosen_site() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, _downloads, _fake) = page_with(&directory);

    assert_eq!(
        harness.config().translation.download_site,
        DownloadSite::HuggingFace,
        "settings with nothing stored start on HuggingFace"
    );
    harness.click_node(&format!("{KEY}.site:trigger"));
    harness.settle(1);
    harness.click_node(&format!("{KEY}.site:panel:list:hit:1"));

    assert_eq!(
        harness.config().translation.download_site,
        DownloadSite::ModelScope,
        "choosing a site stores it at once"
    );
}

#[test]
fn a_failed_site_write_should_keep_the_stored_site() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, _downloads, _fake) = page_with(&directory);
    harness.fail_next_write(crate::session::SettingsWriteError::new(
        app_config_provider::store::ConfigPersistenceError::CreateDirectory {
            path: std::path::PathBuf::from("unwritable"),
            source: std::io::Error::other("the directory cannot be created"),
        },
    ));

    harness.click_node(&format!("{KEY}.site:trigger"));
    harness.settle(1);
    harness.click_node(&format!("{KEY}.site:panel:list:hit:1"));

    assert_eq!(
        harness.config().translation.download_site,
        DownloadSite::HuggingFace,
        "a site that could not be stored must not be published as if it had been"
    );
    assert!(
        harness.find("settings.test.save-failed").is_some(),
        "a write that could not be stored is reported"
    );
}

#[test]
fn the_rows_should_draw_download_retry_progress_and_delete_states() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, downloads, _fake) = page_with(&directory);
    let path = directory.path().to_path_buf();
    downloads.force_downloaded_for_test(TranslationModel::HyMt218BQ4Km, path.clone());
    downloads.set_state_for_test(
        TranslationModel::HyMt218BQ6K,
        path.clone(),
        DownloadState::Downloading {
            downloaded: 100,
            mb_per_second: 1.5,
        },
    );
    downloads.set_state_for_test(
        TranslationModel::HyMt218BQ8,
        path.clone(),
        DownloadState::DownloadFailed {
            failure: DownloadFailure::Transport,
        },
    );
    downloads.set_state_for_test(
        TranslationModel::HyMt218B2Bit,
        path.clone(),
        DownloadState::Deleting,
    );
    downloads.set_state_for_test(
        TranslationModel::HyMt27BQ4Km,
        path.clone(),
        DownloadState::Pausing { downloaded: 100 },
    );
    downloads.set_state_for_test(
        TranslationModel::HyMt27BQ6K,
        path.clone(),
        DownloadState::Paused { downloaded: 100 },
    );
    downloads.set_state_for_test(
        TranslationModel::HyMt218B125Bit,
        path,
        DownloadState::DeleteFailed {
            failure: DeleteFailure::Io {
                kind: std::io::ErrorKind::PermissionDenied,
            },
        },
    );
    harness.settle(2);

    assert!(
        action_drawn(&harness, "DELETE", 0),
        "a downloaded model offers delete"
    );
    assert!(!action_drawn(&harness, "DOWNLOAD", 0));

    assert!(action_drawn(&harness, "PAUSE", 1));
    assert!(action_drawn(&harness, "TERMINATE", 1));
    assert!(!action_drawn(&harness, "DELETE", 1));
    assert!(harness.find("SETTINGS_AI_MODEL_PROGRESS_1").is_some());

    assert!(action_drawn(&harness, "RETRY", 2));
    assert!(action_drawn(&harness, "TERMINATE", 2));
    assert!(!action_drawn(&harness, "DELETE", 2));
    assert!(harness.find("SETTINGS_AI_MODEL_ERROR_2").is_some());

    assert!(action_drawn(&harness, "DELETING", 3));
    assert!(!action_drawn(&harness, "DELETE", 3));

    assert!(action_drawn(&harness, "DELETE", 4));
    assert!(harness.find("SETTINGS_AI_MODEL_ERROR_4").is_some());

    assert!(action_drawn(&harness, "PAUSE", 5));
    assert!(action_drawn(&harness, "TERMINATE", 5));
    assert!(harness.find("SETTINGS_AI_MODEL_PROGRESS_5").is_some());

    assert!(action_drawn(&harness, "RESUME", 6));
    assert!(action_drawn(&harness, "TERMINATE", 6));
    assert!(harness.find("SETTINGS_AI_MODEL_PROGRESS_6").is_some());

    assert!(action_drawn(&harness, "DOWNLOAD", 7));
    assert!(!action_drawn(&harness, "DELETE", 7));
    assert!(harness.find("SETTINGS_AI_MODEL_PROGRESS_7").is_none());
}

#[test]
fn a_download_without_a_chosen_site_should_use_huggingface() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, _downloads, fake) = page_with(&directory);

    harness.click_node("SETTINGS_AI_MODEL_DOWNLOAD_7");

    let adds = fake.adds();
    assert_eq!(adds.len(), 1);
    assert_eq!(
        adds[0].0.url.as_str(),
        TranslationModel::HyMt27BQ8.download_url(DownloadSite::HuggingFace)
    );
}

#[test]
fn clicking_retry_should_follow_the_stored_site() {
    let directory = TempDir::new().expect("the test directory builds");
    let mut config = config_in(directory.path());
    config.translation.download_site = DownloadSite::ModelScope;
    let (mut harness, _downloads, fake) = page(config);
    let model = TranslationModel::HyMt218BQ4Km;
    harness.click_node("SETTINGS_AI_MODEL_DOWNLOAD_0");
    let id = fake
        .added()
        .expect("the download was added to the fake manager");
    fake.push(
        harness.app_mut(),
        download_task(
            id,
            directory.path().join(model.file_name()),
            download_provider::task::DownloadStatus::Failed {
                failure: DownloadFailure::Transport,
            },
        ),
    );
    harness.settle(2);

    harness.click_node("SETTINGS_AI_MODEL_RETRY_0");
    assert!(
        fake.resumed(id),
        "a retry at the site the task came from continues it"
    );

    harness.update_config(|config| config.translation.download_site = DownloadSite::HuggingFace);
    harness.settle(1);
    harness.click_node("SETTINGS_AI_MODEL_RETRY_0");
    assert!(
        fake.deleted(id),
        "a retry at another site removes the failed task first"
    );
}

#[test]
fn a_paused_download_should_be_stopped_from_its_row() {
    let directory = TempDir::new().expect("the test directory builds");
    let model = TranslationModel::HyMt218BQ4Km;
    let part = directory.path().join(format!("{}.part", model.file_name()));
    std::fs::write(&part, b"partial").expect("the partial file is written");
    let (mut harness, downloads, _fake) = page_with(&directory);
    downloads.set_state_for_test(
        model,
        directory.path().to_path_buf(),
        DownloadState::Paused { downloaded: 7 },
    );
    harness.settle(2);

    harness.click_node("SETTINGS_AI_MODEL_TERMINATE_0");
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while part.exists() || downloads.row_state(model, directory.path()) != DownloadState::Idle {
        assert!(
            Instant::now() < deadline,
            "the stopped download never settled"
        );
        harness.settle(1);
        std::thread::yield_now();
    }

    assert!(
        !part.exists(),
        "the stopped download's partial file is gone"
    );
}
#[test]
fn the_rows_should_draw_the_stopping_states() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, downloads, _fake) = page_with(&directory);
    let path = directory.path().to_path_buf();
    downloads.set_state_for_test(
        TranslationModel::HyMt218BQ4Km,
        path.clone(),
        DownloadState::Terminating,
    );
    downloads.set_state_for_test(
        TranslationModel::HyMt218BQ6K,
        path,
        DownloadState::TerminateFailed {
            failure: DeleteFailure::Unload(UnloadFailure::Engine),
        },
    );
    harness.settle(2);

    assert!(action_drawn(&harness, "TERMINATING", 0));
    assert!(!action_drawn(&harness, "TERMINATE", 0));
    assert!(harness.find("SETTINGS_AI_MODEL_PROGRESS_0").is_some());
    assert!(action_drawn(&harness, "TERMINATE", 1));
    assert!(harness.find("SETTINGS_AI_MODEL_ERROR_1").is_some());
    assert!(!action_drawn(&harness, "RETRY", 1));
}

#[test]
fn the_row_text_should_stay_on_one_line() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, downloads, _fake) = page_with(&directory);
    downloads.set_state_for_test(
        TranslationModel::HyMt218BQ6K,
        directory.path().to_path_buf(),
        DownloadState::DownloadFailed {
            failure: DownloadFailure::SizeMismatch {
                expected: 7_981_928_896,
                actual: 7_981_928_895,
            },
        },
    );
    harness.settle(2);

    let error = harness
        .bounds(
            harness
                .find("SETTINGS_AI_MODEL_ERROR_1")
                .expect("the failed download draws its reason"),
        )
        .expect("the reason is laid out");
    assert!(
        error.size.height < 25.0,
        "the long failure reason should be cut to one line: {error:?}"
    );
    let size = harness
        .bounds(
            harness
                .find("SETTINGS_AI_MODEL_SIZE_4")
                .expect("the size is drawn"),
        )
        .expect("the size is laid out");
    assert!(size.size.height < 25.0, "the size stays on one line");
}

#[test]
fn clicking_download_should_use_the_stored_site() {
    let directory = TempDir::new().expect("the test directory builds");
    let mut config = config_in(directory.path());
    config.translation.download_site = DownloadSite::ModelScope;
    let (mut harness, _downloads, fake) = page(config);
    let model = TranslationModel::HyMt27BQ8;

    harness.click_node("SETTINGS_AI_MODEL_DOWNLOAD_7");

    let adds = fake.adds();
    assert_eq!(adds.len(), 1, "the download icon starts one transfer");
    assert_eq!(
        adds[0].0.url.as_str(),
        model.download_url(DownloadSite::ModelScope),
        "the download starts from the site the settings hold"
    );
    assert_eq!(
        adds[0].0.destination,
        directory.path().join(model.file_name())
    );
}

#[test]
fn clicking_resume_should_continue_the_paused_task() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, _downloads, fake) = page_with(&directory);
    let model = TranslationModel::HyMt27BQ6K;
    harness.click_node("SETTINGS_AI_MODEL_DOWNLOAD_6");
    let id = fake
        .added()
        .expect("the download was added to the fake manager");
    fake.push(
        harness.app_mut(),
        download_task(
            id,
            directory.path().join(model.file_name()),
            download_provider::task::DownloadStatus::Paused {
                downloaded: 100,
                total: Some(1000),
            },
        ),
    );
    harness.settle(2);

    harness.click_node("SETTINGS_AI_MODEL_RESUME_6");

    assert!(
        fake.resumed(id),
        "the continue icon resumes the paused task"
    );
}

#[test]
fn clicking_terminate_should_stop_the_running_download() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, downloads, fake) = page_with(&directory);
    let model = TranslationModel::HyMt27BQ6K;
    harness.click_node("SETTINGS_AI_MODEL_DOWNLOAD_6");
    let id = fake
        .added()
        .expect("the download was added to the fake manager");
    fake.push(
        harness.app_mut(),
        download_task(
            id,
            directory.path().join(model.file_name()),
            download_provider::task::DownloadStatus::Downloading {
                downloaded: 100,
                total: Some(1000),
                mb_per_second: 1.0,
            },
        ),
    );
    harness.settle(2);

    harness.click_node("SETTINGS_AI_MODEL_TERMINATE_6");

    assert!(
        fake.deleted(id),
        "stopping a running download removes its task"
    );
    assert_eq!(
        downloads.row_state(model, directory.path()),
        DownloadState::Terminating,
        "the row says it is stopping while the files are removed"
    );
}

#[test]
fn the_downloading_status_should_name_the_speed_before_the_progress() {
    let english = translate_in(LanguageCode::En);
    assert_eq!(
        t_smol!(
            english,
            "settings_ui.ai.downloading_progress",
            speed = "1.5 MB/s",
            percent = 42
        )
        .as_str(),
        "Downloading 1.5 MB/s 42%",
        "the English status names the speed and then the progress"
    );

    let chinese = translate_in(LanguageCode::ZhCn);
    assert_eq!(
        t_smol!(
            chinese,
            "settings_ui.ai.downloading_progress",
            speed = "1.5 MB/s",
            percent = 42
        )
        .as_str(),
        "下载中 1.5 MB/s 42%",
        "the Chinese status is the sentence the page was designed with"
    );
}

#[test]
fn every_row_action_should_keep_its_copy_and_its_icon() {
    for (action, label_key, icon) in [
        (
            RowAction::Download,
            "settings_ui.ai.download",
            IconKind::DownloadSimple,
        ),
        (RowAction::Pause, "settings_ui.ai.pause", IconKind::Pause),
        (RowAction::Resume, "settings_ui.ai.resume", IconKind::Play),
        (
            RowAction::Retry,
            "settings_ui.ai.retry",
            IconKind::ArrowsClockwise,
        ),
        (
            RowAction::Terminate,
            "settings_ui.ai.terminate",
            IconKind::StopFill,
        ),
        (RowAction::Delete, "settings_ui.ai.delete", IconKind::Trash),
        (
            RowAction::Deleting,
            "settings_ui.ai.deleting",
            IconKind::Trash,
        ),
        (
            RowAction::Terminating,
            "settings_ui.ai.terminate",
            IconKind::StopFill,
        ),
    ] {
        assert_eq!(action.label_key(), label_key);
        assert_eq!(action.icon(), icon);
    }
}

#[test]
fn whole_percent_should_measure_a_download_against_the_published_file() {
    let model = TranslationModel::HyMt218BQ4Km;
    let total = model.size_bytes();

    assert_eq!(whole_percent(model, 0), 0);
    assert_eq!(whole_percent(model, total / 2), 50);
    assert_eq!(whole_percent(model, total), 100);
    assert_eq!(whole_percent(model, total + 1), 100);
    assert_eq!(whole_percent(model, u64::MAX), 100);
}

#[test]
fn a_speed_should_be_drawn_in_the_unit_that_keeps_it_short() {
    for (rate, expected) in [
        (0.0, "0 KB/s"),
        (0.5, "512 KB/s"),
        (0.999, "1022.98 KB/s"),
        (1.0, "1 MB/s"),
        (1.234, "1.23 MB/s"),
        (1.235, "1.24 MB/s"),
        (1.5, "1.5 MB/s"),
        (2.0, "2 MB/s"),
        (1024.0, "1024 MB/s"),
        (1536.0, "1.5 GB/s"),
        (2048.0, "2 GB/s"),
    ] {
        assert_eq!(speed_text(rate).as_str(), expected);
    }
}

#[test]
fn a_model_size_should_be_drawn_in_the_unit_that_keeps_it_short() {
    for model in TranslationModel::ALL {
        let text = size_text(model.size_bytes());
        assert!(
            text.ends_with(" MB") || text.ends_with(" GB"),
            "{} is drawn with a unit: {text}",
            model.display_name()
        );
    }
    assert_eq!(size_text(1024 * 1024 * 1024).as_str(), "1024.00 MB");
    assert_eq!(size_text(1024 * 1024 * 1024 + 1).as_str(), "1.00 GB");
}

#[test]
fn releasing_the_page_should_take_its_nodes_away() {
    let directory = TempDir::new().expect("the test directory builds");
    let (mut harness, _downloads, _fake) = page_with(&directory);
    assert!(harness.find("SETTINGS_AI_PAGE").is_some());

    release_page(&mut harness);
    harness.drain();

    assert!(
        harness.find("SETTINGS_AI_PAGE").is_none(),
        "releasing the page removes its nodes"
    );
}

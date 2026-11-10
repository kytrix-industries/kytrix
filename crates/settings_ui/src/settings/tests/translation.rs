//! The tests of the translation page: its directory, its rows, and its downloads.

use app_config::{
    settings::Language,
    translation::{DownloadSite, TranslationModel},
};
use app_config_provider::manager::ConfigManager;
use download_provider::error::DownloadFailure;
use gpui_kit::{BorrowAppContext as _, SharedString, TestAppContext, px, size};

use super::test_support::*;
use crate::translation_downloads::{DeleteFailure, DownloadState};

/// The translation page draws the directory row, the model select, and the table of the
/// eight supported models, in that order.
#[gpui_kit::test]
fn the_translation_page_should_draw_its_models_and_directory_row(cx: &mut TestAppContext) {
    let (_panel, cx) = settings(configured_settings(), cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");

    for selector in [
        "SETTINGS_AI_PAGE",
        "SETTINGS_AI_DIRECTORY",
        "SETTINGS_AI_DIRECTORY_INPUT",
        "SETTINGS_AI_BROWSE",
        "SETTINGS_AI_DOWNLOAD_SITE",
        "SETTINGS_AI_SITE_HUGGINGFACE",
        "SETTINGS_AI_SITE_MODELSCOPE",
        "SETTINGS_AI_MODEL_SELECT_SECTION",
        "SETTINGS_AI_MODEL_SELECT",
        "SETTINGS_AI_MODELS",
        "SETTINGS_AI_MODEL_TABLE",
        "SETTINGS_AI_MODEL_0",
        "SETTINGS_AI_MODEL_1",
        "SETTINGS_AI_MODEL_2",
        "SETTINGS_AI_MODEL_3",
        "SETTINGS_AI_MODEL_4",
        "SETTINGS_AI_MODEL_5",
        "SETTINGS_AI_MODEL_6",
        "SETTINGS_AI_MODEL_7",
        "SETTINGS_AI_MODEL_DOWNLOAD_0",
        "SETTINGS_AI_MODEL_DOWNLOAD_7",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "the page should draw {selector}"
        );
    }

    let directory = cx
        .debug_bounds("SETTINGS_AI_DIRECTORY")
        .expect("the directory row should be drawn");
    let sites = cx
        .debug_bounds("SETTINGS_AI_DOWNLOAD_SITE")
        .expect("the download site row should be drawn");
    let select = cx
        .debug_bounds("SETTINGS_AI_MODEL_SELECT_SECTION")
        .expect("the model select should be drawn");
    let models = cx
        .debug_bounds("SETTINGS_AI_MODELS")
        .expect("the model list should be drawn");
    let table = cx
        .debug_bounds("SETTINGS_AI_MODEL_TABLE")
        .expect("the model table should be drawn");

    assert!(directory.bottom() <= sites.top());
    assert!(sites.bottom() <= select.top());
    assert!(select.bottom() <= models.top());
    assert!(
        table.top() >= models.top(),
        "the table is what the model section holds"
    );

    let rows = (0..8)
        .map(|index| {
            cx.debug_bounds(Box::leak(
                format!("SETTINGS_AI_MODEL_{index}").into_boxed_str(),
            ))
            .unwrap_or_else(|| panic!("the table should draw model row {index}"))
        })
        .collect::<Vec<_>>();
    for (index, pair) in rows.windows(2).enumerate() {
        assert!(
            pair[0].top() < pair[1].top(),
            "the table draws the models in page order: row {index} is above row {}",
            index + 1
        );
    }
}

/// At the window's minimum size the translation page still draws its table in the normal
/// content column, and every cell stays on the single line it is drawn with: the size and
/// the longest failure reason are cut rather than stacked.
#[gpui_kit::test]
fn the_translation_table_should_be_drawn_at_the_minimum_window_size(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    cx.simulate_resize(size(px(1280.0), px(960.0)));
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();

    let table = cx
        .debug_bounds("SETTINGS_AI_MODEL_TABLE")
        .expect("the table should be drawn at the window's minimum size");
    assert!(
        table.size.width >= px(600.0),
        "the table should fill the content column beside the sidebar: {table:?}"
    );
    for index in 0..8 {
        let selector = Box::leak(format!("SETTINGS_AI_MODEL_{index}").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_some(),
            "model row {index} should be drawn at the window's minimum size"
        );
    }

    let size_cell = cx
        .debug_bounds("SETTINGS_AI_MODEL_SIZE_4")
        .expect("the 1.25-bit size should be drawn");
    assert!(
        size_cell.size.height < px(25.0),
        "the size should stay on one line: {size_cell:?}"
    );

    // A size mismatch with a realistic pair of sizes is the longest reason the status column
    // ever shows, and it should be cut to one line rather than wrap.
    downloads.update(cx, |downloads, cx| {
        downloads.set_state_for_test(
            TranslationModel::HyMt218BQ6K,
            path,
            DownloadState::DownloadFailed {
                failure: DownloadFailure::SizeMismatch {
                    expected: 7_981_928_896,
                    actual: 7_981_928_895,
                },
            },
            cx,
        );
    });
    settle(cx);

    let error = cx
        .debug_bounds("SETTINGS_AI_MODEL_ERROR_1")
        .expect("the failed download should draw its reason");
    assert!(
        error.size.height < px(25.0),
        "the long failure reason should be cut to one line: {error:?}"
    );
}

/// The system folder picker is what writes the directory field, and the directory it writes
/// is stored at once.
#[gpui_kit::test]
fn browsing_should_store_the_chosen_translation_directory(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let directory = tempfile::tempdir().expect("the test directory should build");

    click(cx, "SETTINGS_AI_BROWSE");
    assert!(
        cx.did_prompt_for_paths(),
        "the browse button should open the system folder prompt"
    );
    cx.simulate_path_prompt_response(|options| {
        assert!(!options.files, "the prompt should not offer files");
        assert!(options.directories, "the prompt should offer directories");
        assert!(!options.multiple, "one directory is enough");
        Some(vec![directory.path().to_path_buf()])
    });
    settle(cx);

    let expected = SharedString::from(directory.path().to_string_lossy().into_owned());
    assert_eq!(
        cx.read(|cx| panel.read_with(cx, |panel, cx| {
            panel.translation.read(cx).shown_directory(cx)
        })),
        Some(expected),
        "the field should show the chosen directory"
    );
    assert_eq!(
        stored(cx).translation.model_directory,
        Some(directory.path().to_path_buf()),
        "the directory should be stored as soon as it is chosen"
    );
}

/// A page that opens over a stored directory shows it in the field.
#[gpui_kit::test]
fn a_stored_translation_directory_should_be_shown(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");

    let expected = SharedString::from(directory.path().to_string_lossy().into_owned());
    assert_eq!(
        cx.read(|cx| panel.read_with(cx, |panel, cx| {
            panel.translation.read(cx).shown_directory(cx)
        })),
        Some(expected)
    );
}

/// The directory field is read-only: typing into it leaves the value the browse button chose.
#[gpui_kit::test]
fn the_translation_directory_should_not_be_editable(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");

    let expected = SharedString::from(directory.path().to_string_lossy().into_owned());
    type_into(cx, "SETTINGS_AI_DIRECTORY_INPUT", "E:\\typed");

    assert_eq!(
        cx.read(|cx| panel.read_with(cx, |dialog, cx| {
            dialog.translation.read(cx).shown_directory(cx)
        })),
        Some(expected)
    );
}

/// The status column of each row follows the download state: buttons only where the state
/// has an action, and the text of the state beside them.
#[gpui_kit::test]
fn the_translation_rows_should_draw_download_retry_progress_and_delete_states(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(TranslationModel::HyMt218BQ4Km, path.clone(), cx);
        downloads.set_state_for_test(
            TranslationModel::HyMt218BQ6K,
            path.clone(),
            DownloadState::Downloading {
                downloaded: 100,
                mb_per_second: 1.5,
            },
            cx,
        );
        downloads.set_state_for_test(
            TranslationModel::HyMt218BQ8,
            path.clone(),
            DownloadState::DownloadFailed {
                failure: DownloadFailure::Transport,
            },
            cx,
        );
        downloads.set_state_for_test(
            TranslationModel::HyMt218B2Bit,
            path.clone(),
            DownloadState::Deleting,
            cx,
        );
        downloads.set_state_for_test(
            TranslationModel::HyMt27BQ4Km,
            path.clone(),
            DownloadState::Pausing { downloaded: 100 },
            cx,
        );
        downloads.set_state_for_test(
            TranslationModel::HyMt27BQ6K,
            path.clone(),
            DownloadState::Paused { downloaded: 100 },
            cx,
        );
        downloads.set_state_for_test(
            TranslationModel::HyMt218B125Bit,
            path,
            DownloadState::DeleteFailed {
                failure: DeleteFailure::Io {
                    kind: std::io::ErrorKind::PermissionDenied,
                },
            },
            cx,
        );
    });
    settle(cx);

    // A model that was never started but whose file is present shows the downloaded row.
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_0").is_some(),
        "a downloaded model offers delete"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DOWNLOAD_0").is_none(),
        "a downloaded model offers no download button"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PROGRESS_0").is_none()
            && cx.debug_bounds("SETTINGS_AI_MODEL_ERROR_0").is_none(),
        "a downloaded model has no status text"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PAUSE_1").is_some(),
        "a download in flight offers pause"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_TERMINATE_1").is_some(),
        "a download in flight can be stopped for good"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_1").is_none(),
        "a download in flight is stopped rather than deleted"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PROGRESS_1").is_some(),
        "a download in flight draws how far it is"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DOWNLOAD_1").is_none(),
        "a download in flight offers no download button"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_RETRY_2").is_some(),
        "a failed download offers retry"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_TERMINATE_2").is_some(),
        "a failed download can be stopped for good"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_2").is_none(),
        "a failed download is stopped rather than deleted"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_ERROR_2").is_some(),
        "a failed download names its reason"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETING_3").is_some(),
        "a delete in flight offers its disabled button"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_3").is_none(),
        "a delete in flight must not offer a second delete"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PROGRESS_3").is_none()
            && cx.debug_bounds("SETTINGS_AI_MODEL_ERROR_3").is_none(),
        "a delete in flight has no status text"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_ERROR_4").is_some(),
        "a failed delete names its reason even when the file is gone"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_4").is_some(),
        "a failed delete can be tried again"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DOWNLOAD_4").is_none(),
        "a failed delete offers its own action rather than a download"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PAUSE_5").is_some(),
        "a pausing download is drawn with its disabled pause"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_TERMINATE_5").is_some(),
        "a pausing download can still be stopped for good"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_5").is_none(),
        "a pausing download is stopped rather than deleted"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PROGRESS_5").is_some(),
        "a pausing download says what it is doing"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_RESUME_5").is_none(),
        "a pausing download cannot be resumed yet"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_RESUME_6").is_some(),
        "a paused download offers resume"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_TERMINATE_6").is_some(),
        "a paused download can be stopped for good"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PAUSE_6").is_none(),
        "a paused download offers no pause"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PROGRESS_6").is_some(),
        "a paused download says how much it already has"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_6").is_none(),
        "a paused download is stopped rather than deleted"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DOWNLOAD_7").is_some(),
        "a model that was never downloaded offers download"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_7").is_none()
            && cx.debug_bounds("SETTINGS_AI_MODEL_PROGRESS_7").is_none()
            && cx.debug_bounds("SETTINGS_AI_MODEL_ERROR_7").is_none(),
        "an idle model has no delete and no status text"
    );
}

/// A row whose download is being stopped draws the disabled stop and says so, and a stop
/// that failed offers the stop again rather than a retry of the download.
#[gpui_kit::test]
fn the_translation_rows_should_draw_the_stopping_states(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();

    downloads.update(cx, |downloads, cx| {
        downloads.set_state_for_test(
            TranslationModel::HyMt218BQ4Km,
            path.clone(),
            DownloadState::Terminating,
            cx,
        );
        downloads.set_state_for_test(
            TranslationModel::HyMt218BQ6K,
            path,
            DownloadState::TerminateFailed {
                failure: DeleteFailure::Io {
                    kind: std::io::ErrorKind::PermissionDenied,
                },
            },
            cx,
        );
    });
    settle(cx);

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_TERMINATING_0").is_some(),
        "a download being stopped offers its disabled stop"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_TERMINATE_0").is_none(),
        "a download being stopped must not offer a second stop"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_PROGRESS_0").is_some(),
        "a download being stopped says what it is doing"
    );

    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_TERMINATE_1").is_some(),
        "a stop that failed can be tried again"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_ERROR_1").is_some(),
        "a failed stop names its reason"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_RETRY_1").is_none()
            && cx.debug_bounds("SETTINGS_AI_MODEL_DELETE_1").is_none(),
        "a failed stop offers neither a retry nor a delete"
    );
}

/// Choosing a site is a change of the session, and the download icon starts from the site
/// the session holds rather than asking for one.
#[gpui_kit::test]
fn clicking_download_should_start_from_the_site_the_session_holds(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();
    let model = TranslationModel::HyMt27BQ8;

    // The seam records downloads instead of transferring them, so nothing leaves the
    // process. It needs a row of its own, which is not the one this test downloads.
    downloads.update(cx, |downloads, cx| {
        downloads.set_transfers_deferred_for_test(
            TranslationModel::HyMt27BQ6K,
            path.clone(),
            DownloadSite::HuggingFace,
            cx,
        );
    });
    settle(cx);

    click(cx, "SETTINGS_AI_SITE_MODELSCOPE");
    assert_eq!(
        stored(cx).translation.download_site,
        DownloadSite::ModelScope,
        "choosing a site stores it at once"
    );

    click(cx, "SETTINGS_AI_MODEL_DOWNLOAD_7");

    assert_eq!(
        cx.read(|cx| downloads.read(cx).site(model, &path)),
        Some(DownloadSite::ModelScope),
        "the download starts from the site the session holds"
    );
    assert_eq!(
        cx.read(|cx| downloads.read(cx).row_state(model, &path)),
        DownloadState::Downloading {
            downloaded: 0,
            mb_per_second: 0.0,
        },
        "the download icon starts the download without asking for a site"
    );
}

/// A session that never chose a site downloads from HuggingFace.
#[gpui_kit::test]
fn a_download_without_a_chosen_site_should_use_huggingface(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();
    let model = TranslationModel::HyMt27BQ8;

    downloads.update(cx, |downloads, cx| {
        downloads.set_transfers_deferred_for_test(
            TranslationModel::HyMt27BQ6K,
            path.clone(),
            DownloadSite::HuggingFace,
            cx,
        );
    });
    settle(cx);

    click(cx, "SETTINGS_AI_MODEL_DOWNLOAD_7");

    assert_eq!(
        cx.read(|cx| downloads.read(cx).site(model, &path)),
        Some(DownloadSite::HuggingFace),
        "a session with no choice of its own downloads from HuggingFace"
    );
    assert_eq!(
        cx.read(|cx| downloads.read(cx).row_state(model, &path)),
        DownloadState::Downloading {
            downloaded: 0,
            mb_per_second: 0.0,
        }
    );
}

/// A retry at the site the failed download came from continues that task from its partial
/// file.
#[gpui_kit::test]
fn a_retry_at_the_site_of_the_failed_download_should_continue_it(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();
    let model = TranslationModel::HyMt218BQ8;

    downloads.update(cx, |downloads, cx| {
        downloads.set_transfers_deferred_for_test(
            TranslationModel::HyMt27BQ8,
            path.clone(),
            DownloadSite::HuggingFace,
            cx,
        );
        // A download that came from ModelScope and failed, which is the row a retry starts
        // from.
        downloads.download(model, path.clone(), DownloadSite::ModelScope, cx);
        downloads.set_state_for_test(
            model,
            path.clone(),
            DownloadState::DownloadFailed {
                failure: DownloadFailure::Transport,
            },
            cx,
        );
    });
    settle(cx);

    click(cx, "SETTINGS_AI_SITE_MODELSCOPE");
    click(cx, "SETTINGS_AI_MODEL_RETRY_2");

    assert_eq!(
        cx.read(|cx| downloads.read(cx).site(model, &path)),
        Some(DownloadSite::ModelScope),
        "the retry uses the site the session holds"
    );
    assert_eq!(
        cx.read(|cx| downloads.read(cx).row_state(model, &path)),
        DownloadState::Downloading {
            downloaded: 0,
            mb_per_second: 0.0,
        },
        "the site of the failed task continues it rather than discarding it"
    );
}

/// A retry whose session holds another site discards the failed transfer and starts again
/// from the chosen site.
#[gpui_kit::test]
fn a_retry_at_another_site_should_restart(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();
    let model = TranslationModel::HyMt218BQ8;

    downloads.update(cx, |downloads, cx| {
        downloads.set_transfers_deferred_for_test(
            TranslationModel::HyMt27BQ8,
            path.clone(),
            DownloadSite::HuggingFace,
            cx,
        );
        downloads.download(model, path.clone(), DownloadSite::ModelScope, cx);
        downloads.set_state_for_test(
            model,
            path.clone(),
            DownloadState::DownloadFailed {
                failure: DownloadFailure::Transport,
            },
            cx,
        );
    });
    settle(cx);

    // The session still holds HuggingFace while the failed task came from ModelScope.
    click(cx, "SETTINGS_AI_MODEL_RETRY_2");
    settle(cx);

    assert_eq!(
        cx.read(|cx| downloads.read(cx).site(model, &path)),
        Some(DownloadSite::HuggingFace),
        "the restart is recorded against the site the session holds"
    );
    assert_eq!(
        cx.read(|cx| downloads.read(cx).row_state(model, &path)),
        DownloadState::Downloading {
            downloaded: 0,
            mb_per_second: 0.0,
        },
        "the other site restarts the download from zero"
    );
}

/// Continuing a paused download starts it where it is, whatever site the session holds.
#[gpui_kit::test]
fn a_paused_download_should_resume_where_it_is(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();
    let model = TranslationModel::HyMt27BQ6K;

    downloads.update(cx, |downloads, cx| {
        downloads.set_transfers_deferred_for_test(
            TranslationModel::HyMt27BQ8,
            path.clone(),
            DownloadSite::HuggingFace,
            cx,
        );
        downloads.set_state_for_test(
            model,
            path.clone(),
            DownloadState::Paused { downloaded: 100 },
            cx,
        );
    });
    settle(cx);

    click(cx, "SETTINGS_AI_SITE_MODELSCOPE");
    click(cx, "SETTINGS_AI_MODEL_RESUME_6");

    assert_eq!(
        cx.read(|cx| downloads.read(cx).row_state(model, &path)),
        DownloadState::Downloading {
            downloaded: 0,
            mb_per_second: 0.0,
        },
        "the continue icon resumes the paused task"
    );
}

/// Stopping a paused download from its row discards what it wrote: the partial file goes and
/// the row offers a download again.
#[gpui_kit::test]
fn a_paused_download_should_be_stopped_from_its_row(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();
    let model = TranslationModel::HyMt27BQ6K;
    let part = path.join(format!("{}.part", model.file_name()));
    std::fs::write(&part, b"partial").expect("the partial file should be written");

    downloads.update(cx, |downloads, cx| {
        downloads.set_state_for_test(
            model,
            path.clone(),
            DownloadState::Paused { downloaded: 7 },
            cx,
        );
    });
    settle(cx);

    click(cx, "SETTINGS_AI_MODEL_TERMINATE_6");
    wait_until_absent(cx, &downloads, model, &path, &part);

    assert!(
        !part.exists(),
        "the stopped download's partial file is gone"
    );
    assert_eq!(
        cx.read(|cx| downloads.read(cx).row_state(model, &path)),
        DownloadState::Idle,
        "the stopped download is a download the row offers again"
    );
}

/// Opening the settings page without entering the translation page writes no translation
/// repair, even when a download left a directory to adopt.
#[gpui_kit::test]
fn opening_the_settings_should_not_write_the_translation_repair(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let (panel, cx) = settings(configured_settings(), cx);
    let downloads = translation_downloads(cx);
    downloads.update(cx, |downloads, _| {
        downloads.set_last_download_directory_for_test(directory.path().to_path_buf());
    });
    settle(cx);

    assert_ne!(
        current_page(&panel, cx),
        "ai.translation",
        "the settings open on another page"
    );
    assert_eq!(
        stored(cx).translation.model_directory,
        None,
        "opening the settings page must not write the translation settings"
    );
}

/// Entering the translation page adopts the directory once: the repair is written through the
/// session, the field shows it, and entering the page again does not write it a second time.
#[gpui_kit::test]
fn entering_the_translation_page_should_repair_once(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let (panel, cx) = settings(configured_settings(), cx);
    let downloads = translation_downloads(cx);
    downloads.update(cx, |downloads, cx| {
        downloads.complete_download_for_test(
            TranslationModel::HyMt27BQ8,
            directory.path().to_path_buf(),
            cx,
        );
    });
    settle(cx);

    click(cx, "SETTINGS_ENTRY_ai.translation");
    settle(cx);

    let expected = SharedString::from(directory.path().to_string_lossy().into_owned());
    assert_eq!(
        stored(cx).translation.model_directory,
        Some(directory.path().to_path_buf()),
        "entering the page should adopt the directory the download finished in"
    );
    assert_eq!(
        cx.read(|cx| panel.read_with(cx, |panel, cx| {
            panel.translation.read(cx).shown_directory(cx)
        })),
        Some(expected),
        "the read-only field should show the directory the repair stored"
    );

    click(cx, "SETTINGS_ENTRY_appearance");
    cx.update(|_, cx| {
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.translation.model_directory = None, cx);
        });
    });
    click(cx, "SETTINGS_ENTRY_ai.translation");
    settle(cx);

    assert_eq!(
        stored(cx).translation.model_directory,
        None,
        "the repair is made the first time the page is entered and never again"
    );
}

/// A directory that could not be stored is not shown as if it had been: the field keeps the
/// stored directory, the page reports why, and the choice is stored when it is made again.
#[gpui_kit::test]
fn a_failed_directory_write_should_keep_the_stored_directory_and_report(cx: &mut TestAppContext) {
    let stored_directory = tempfile::tempdir().expect("the stored directory should build");
    let chosen_directory = tempfile::tempdir().expect("the chosen directory should build");
    let (mut manager, blocking, _storage) = manager_behind_a_file(cx);
    cx.update(|cx| {
        manager.update(
            |config| {
                config.translation.model_directory = Some(stored_directory.path().to_path_buf());
            },
            cx,
        );
    });
    let (panel, cx) = settings_over(manager, Language::En, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");

    let stored_shown = SharedString::from(stored_directory.path().to_string_lossy().into_owned());
    assert_eq!(
        cx.read(|cx| panel.read_with(cx, |panel, cx| {
            panel.translation.read(cx).shown_directory(cx)
        })),
        Some(stored_shown.clone()),
        "the page should open on the directory the settings hold"
    );

    click(cx, "SETTINGS_AI_BROWSE");
    cx.simulate_path_prompt_response(|_| Some(vec![chosen_directory.path().to_path_buf()]));
    settle(cx);

    assert_eq!(
        stored(cx).translation.model_directory,
        Some(stored_directory.path().to_path_buf()),
        "a directory that could not be stored must not be published as if it had been"
    );
    assert_eq!(
        cx.read(|cx| panel.read_with(cx, |panel, cx| {
            panel.translation.read(cx).shown_directory(cx)
        })),
        Some(stored_shown),
        "the field should keep showing the directory that is stored"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "a write that could not be stored should tell the user"
    );

    click(cx, "BASE_UI_ALERT_OK");
    std::fs::remove_file(&blocking).expect("the blocking file should be removable");

    click(cx, "SETTINGS_AI_BROWSE");
    cx.simulate_path_prompt_response(|_| Some(vec![chosen_directory.path().to_path_buf()]));
    settle(cx);

    let chosen_shown = SharedString::from(chosen_directory.path().to_string_lossy().into_owned());
    assert_eq!(
        stored(cx).translation.model_directory,
        Some(chosen_directory.path().to_path_buf()),
        "the retry should store the directory the user chose"
    );
    assert_eq!(
        cx.read(|cx| panel.read_with(cx, |panel, cx| {
            panel.translation.read(cx).shown_directory(cx)
        })),
        Some(chosen_shown),
        "the field should show the directory the retry stored"
    );
}

/// A download site that could not be stored leaves the stored site as it was and reports why;
/// once the settings can be stored, the same choice is written.
#[gpui_kit::test]
fn a_failed_site_write_should_keep_the_stored_site_and_report(cx: &mut TestAppContext) {
    let (manager, blocking, _storage) = manager_behind_a_file(cx);
    let (_panel, cx) = settings_over(manager, Language::En, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");

    assert_eq!(
        stored(cx).translation.download_site,
        DownloadSite::HuggingFace,
        "settings with nothing stored start on HuggingFace"
    );

    click(cx, "SETTINGS_AI_SITE_MODELSCOPE");

    assert_eq!(
        stored(cx).translation.download_site,
        DownloadSite::HuggingFace,
        "a site that could not be stored must not be published as if it had been"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "a write that could not be stored should tell the user"
    );

    click(cx, "BASE_UI_ALERT_OK");
    std::fs::remove_file(&blocking).expect("the blocking file should be removable");

    click(cx, "SETTINGS_AI_SITE_MODELSCOPE");

    assert_eq!(
        stored(cx).translation.download_site,
        DownloadSite::ModelScope,
        "the retry should store the site the user chose"
    );
}

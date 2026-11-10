//! The tests of the translation models: what is offered, selected, and deleted.

use app_config::{
    settings::Language,
    translation::{DownloadSite, TranslationModel},
};
use app_config_provider::manager::ConfigManager;
use gpui_kit::{BorrowAppContext as _, TestAppContext};

use super::test_support::*;
use crate::translation_downloads::DownloadState;

/// The chosen site is stored as soon as it is chosen.
#[gpui_kit::test]
fn choosing_a_download_site_should_store_it_at_once(cx: &mut TestAppContext) {
    let (_panel, cx) = settings(configured_settings(), cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");

    assert_eq!(
        stored(cx).translation.download_site,
        DownloadSite::HuggingFace,
        "settings with nothing stored start on HuggingFace"
    );

    click(cx, "SETTINGS_AI_SITE_SELECT");
    click(cx, "TEST_MENU_ITEM_model_scope");

    assert_eq!(
        stored(cx).translation.download_site,
        DownloadSite::ModelScope,
        "choosing a site should store it at once"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_SITE_SELECT").is_some(),
        "the page should still draw the chosen site"
    );
}

/// The select offers exactly the models whose files are present at their published size.
#[gpui_kit::test]
fn the_model_select_should_list_only_downloaded_models(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(TranslationModel::HyMt27BQ8, path.clone(), cx);
        downloads.force_downloaded_for_test(TranslationModel::HyMt218BQ6K, path.clone(), cx);
    });
    settle(cx);

    let offered = cx.read(|cx| downloads.read(cx).downloaded_models(&path));

    assert_eq!(
        offered,
        vec![TranslationModel::HyMt218BQ6K, TranslationModel::HyMt27BQ8],
        "only the downloaded models are offered, in page order"
    );
    assert!(
        cx.debug_bounds("SETTINGS_AI_MODEL_SELECT").is_some(),
        "the select is drawn over what it offers"
    );
}

/// Choosing a model from the select stores the selection at once.
#[gpui_kit::test]
fn choosing_a_model_should_store_the_selection_at_once(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let model = TranslationModel::HyMt218BQ4Km;
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(model, directory.path().to_path_buf(), cx);
    });
    settle(cx);

    click(cx, "SETTINGS_AI_MODEL_SELECT");
    assert!(
        cx.debug_bounds("TEST_MENU_ITEM_HY-MT2-1.8B-Q4_K_M")
            .is_some(),
        "the downloaded model should be offered"
    );
    click(cx, "TEST_MENU_ITEM_HY-MT2-1.8B-Q4_K_M");

    assert_eq!(
        stored(cx).translation.selected_model,
        Some(model),
        "choosing a model should store it at once"
    );
}

/// A model whose files are being removed is not offered by the model select: it is not one
/// the page can load, so it is not one the user can pick.
#[gpui_kit::test]
fn a_model_being_deleted_should_not_be_offered_by_the_model_select(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let removed = TranslationModel::HyMt218BQ4Km;
    let kept = TranslationModel::HyMt27BQ8;
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(removed, path.clone(), cx);
        downloads.force_downloaded_for_test(kept, path.clone(), cx);
        downloads.set_state_for_test(removed, path, DownloadState::Deleting, cx);
    });
    settle(cx);

    click(cx, "SETTINGS_AI_MODEL_SELECT");

    assert!(
        cx.debug_bounds("TEST_DROPDOWN_PANEL").is_some(),
        "the model select should open over its choices"
    );
    assert!(
        cx.debug_bounds("TEST_MENU_ITEM_HY-MT2-1.8B-Q4_K_M")
            .is_none(),
        "a model being removed must not be offered"
    );
    assert!(
        cx.debug_bounds("TEST_MENU_ITEM_HY-MT2-7B-Q8_0").is_some(),
        "a downloaded model that is staying must be offered"
    );
}

/// Deleting a model clears the selection that named it when the removal starts, before the file
/// goes.
#[gpui_kit::test]
fn deleting_a_selected_model_should_clear_the_selection_and_the_file(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let model = TranslationModel::HyMt218BQ4Km;
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"stub").expect("the fixture file should be written");

    // The unload is held open, so the selection cleared when the removal starts can be told
    // apart from the one the finish clears again.
    let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let unload: std::sync::Arc<
        dyn Fn(&std::path::Path) -> Result<(), crate::translation_downloads::UnloadFailure>
            + Send
            + Sync,
    > = std::sync::Arc::new(move |_: &std::path::Path| {
        entered_sender
            .send(())
            .expect("the test should be waiting for the unload");
        release_receiver
            .lock()
            .expect("the release channel is not poisoned")
            .recv()
            .expect("the test should release the unload");
        Ok(())
    });
    cx.update(|cx| install_translation_downloads_with(cx, unload));

    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    config.translation.selected_model = Some(model);
    let (_panel, cx) = settings(config, cx);
    let downloads = translation_downloads(cx);
    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(model, directory.path().to_path_buf(), cx);
    });
    click(cx, "SETTINGS_ENTRY_ai.translation");
    settle(cx);

    assert_eq!(
        stored(cx).translation.selected_model,
        Some(model),
        "the test starts with the model selected, so the delete is what clears it"
    );

    click(cx, "SETTINGS_AI_MODEL_DELETE_0");
    entered_receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the delete should reach its unload");

    assert_eq!(
        stored(cx).translation.selected_model,
        None,
        "the stored selection is cleared when the removal starts, before the file goes"
    );

    release_sender
        .send(())
        .expect("the delete worker should still be waiting");
    wait_until_absent(cx, &downloads, model, directory.path(), &path);

    assert!(!path.exists(), "the model file is deleted");
    assert_eq!(
        stored(cx).translation.selected_model,
        None,
        "the selection stays cleared once the file is gone"
    );
}

/// A menu that was open before a delete started can still be clicked after it finished: the
/// choice is checked again and a model that is no longer there clears the selection rather
/// than being written back.
#[gpui_kit::test]
fn a_stale_model_menu_choice_after_deletion_should_clear_the_selection(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let removed = TranslationModel::HyMt218BQ4Km;
    let selected = TranslationModel::HyMt27BQ8;
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    config.translation.selected_model = Some(selected);
    let (panel, cx) = settings(config, cx);
    let downloads = translation_downloads(cx);
    let path = directory.path().to_path_buf();

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(removed, path.clone(), cx);
        downloads.force_downloaded_for_test(selected, path.clone(), cx);
    });
    click(cx, "SETTINGS_ENTRY_ai.translation");
    settle(cx);

    // The menu was drawn with the model in it before the removal started ...
    assert!(
        cx.read(|cx| downloads.read(cx).downloaded_models(&path))
            .contains(&removed),
        "the model is offered before the removal starts"
    );
    downloads.update(cx, |downloads, cx| {
        downloads.set_state_for_test(removed, path, DownloadState::Deleting, cx);
    });
    settle(cx);

    // ... and the click lands after it: what was offered is checked against the directory
    // again, and refused.
    panel.update(cx, |panel, cx| {
        panel.translation_for_test().update(cx, |page, cx| {
            page.select_model_for_test(removed, cx);
        });
    });
    settle(cx);

    assert_eq!(
        stored(cx).translation.selected_model,
        None,
        "a menu choice that names a model being removed clears the selection"
    );
}

/// A selection made while the removal was already running is cleared when the removal
/// finishes: by then the files are gone, so the selection cannot stay.
#[gpui_kit::test]
fn a_model_selected_while_it_is_being_deleted_should_be_cleared_when_the_delete_finishes(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let model = TranslationModel::HyMt218BQ4Km;
    let path = directory.path().join(model.file_name());
    std::fs::write(&path, b"stub").expect("the fixture file should be written");

    let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let unload: std::sync::Arc<
        dyn Fn(&std::path::Path) -> Result<(), crate::translation_downloads::UnloadFailure>
            + Send
            + Sync,
    > = std::sync::Arc::new(move |_: &std::path::Path| {
        entered_sender
            .send(())
            .expect("the test should be waiting for the unload");
        release_receiver
            .lock()
            .expect("the release channel is not poisoned")
            .recv()
            .expect("the test should release the unload");
        Ok(())
    });
    cx.update(|cx| install_translation_downloads_with(cx, unload));

    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(model, directory.path().to_path_buf(), cx);
    });
    settle(cx);

    click(cx, "SETTINGS_AI_MODEL_DELETE_0");
    entered_receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the delete should reach its unload");

    // What a selection written while the removal is running looks like: it was not there
    // when the removal started to look, so only the finish can clear it.
    cx.update(|_, cx| {
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.translation.selected_model = Some(model), cx);
        });
    });
    assert_eq!(
        stored(cx).translation.selected_model,
        Some(model),
        "the selection is written while the removal is running"
    );

    release_sender
        .send(())
        .expect("the delete worker should still be waiting");
    wait_until_absent(cx, &downloads, model, directory.path(), &path);

    assert_eq!(
        stored(cx).translation.selected_model,
        None,
        "the finished delete must clear the selection that still names the removed model"
    );
}

/// Deleting a model that is not the selected one leaves the selection alone.
#[gpui_kit::test]
fn deleting_another_model_should_keep_the_selected_one(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let selected = TranslationModel::HyMt218BQ4Km;
    let other = TranslationModel::HyMt27BQ8;
    let selected_path = directory.path().join(selected.file_name());
    std::fs::write(&selected_path, b"stub").expect("the fixture file should be written");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    config.translation.selected_model = Some(selected);
    let (_panel, cx) = settings(config, cx);
    let downloads = translation_downloads(cx);

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(selected, directory.path().to_path_buf(), cx);
        downloads.force_downloaded_for_test(other, directory.path().to_path_buf(), cx);
    });
    click(cx, "SETTINGS_ENTRY_ai.translation");
    settle(cx);

    click(cx, "SETTINGS_AI_MODEL_DELETE_7");

    assert_eq!(
        stored(cx).translation.selected_model,
        Some(selected),
        "deleting another model must not clear the selected one"
    );

    click(cx, "SETTINGS_AI_MODEL_DELETE_0");
    wait_until_absent(cx, &downloads, selected, directory.path(), &selected_path);

    assert_eq!(stored(cx).translation.selected_model, None);
}

/// A finished download becomes the stored selection when nothing usable was selected.
#[gpui_kit::test]
fn a_completed_download_should_select_the_stored_model(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let model = TranslationModel::HyMt218BQ8;

    downloads.update(cx, |downloads, cx| {
        downloads.complete_download_for_test(model, directory.path().to_path_buf(), cx);
    });
    settle(cx);

    assert_eq!(
        stored(cx).translation.selected_model,
        Some(model),
        "the finished download is what the page now uses"
    );
}

/// A directory change clears a selection whose file the new directory does not hold.
#[gpui_kit::test]
fn changing_the_directory_should_clear_the_selection_it_does_not_hold(cx: &mut TestAppContext) {
    let first = tempfile::tempdir().expect("the first directory should build");
    let second = tempfile::tempdir().expect("the second directory should build");
    let mut config = configured_settings();
    config.translation.model_directory = Some(first.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);
    let model = TranslationModel::HyMt218BQ4Km;

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(model, first.path().to_path_buf(), cx);
    });
    cx.update(|_, cx| {
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.translation.selected_model = Some(model), cx);
        });
    });
    settle(cx);

    click(cx, "SETTINGS_AI_BROWSE");
    cx.simulate_path_prompt_response(|_| Some(vec![second.path().to_path_buf()]));
    settle(cx);

    let stored = stored(cx);
    assert_eq!(
        stored.translation.selected_model, None,
        "the model is not in the chosen directory, so the choice goes"
    );
    assert_eq!(
        stored.translation.model_directory,
        Some(second.path().to_path_buf())
    );
}

/// A selection the background manager made in the stored directory is cleared when the user
/// chooses another directory.
#[gpui_kit::test]
fn a_background_selection_should_be_cleared_by_the_directory_change(cx: &mut TestAppContext) {
    let stored_directory = tempfile::tempdir().expect("the stored directory should build");
    let chosen_directory = tempfile::tempdir().expect("the chosen directory should build");
    let selected = TranslationModel::HyMt27BQ8;
    let mut config = configured_settings();
    config.translation.model_directory = Some(stored_directory.path().to_path_buf());
    let (_panel, cx) = settings(config, cx);
    click(cx, "SETTINGS_ENTRY_ai.translation");
    let downloads = translation_downloads(cx);

    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(selected, stored_directory.path().to_path_buf(), cx);
    });
    // What the download manager writes when a download finishes while the page is open: it
    // selects the model in the directory the settings still name.
    cx.update(|_, cx| {
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(
                |config| config.translation.selected_model = Some(selected),
                cx,
            );
        });
    });
    settle(cx);

    click(cx, "SETTINGS_AI_BROWSE");
    cx.simulate_path_prompt_response(|_| Some(vec![chosen_directory.path().to_path_buf()]));
    settle(cx);

    let stored = stored(cx);
    assert_eq!(
        stored.translation.model_directory,
        Some(chosen_directory.path().to_path_buf()),
        "the directory the user chose should be stored"
    );
    assert_eq!(
        stored.translation.selected_model, None,
        "a selection that the new directory does not hold must be cleared with it"
    );
}

/// A model choice that could not be stored leaves the stored selection as it was and reports
/// why; once the settings can be stored, the same choice is written.
#[gpui_kit::test]
fn a_failed_model_selection_should_keep_the_stored_selection_and_report(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("the test directory should build");
    let model = TranslationModel::HyMt218BQ4Km;
    let selected = TranslationModel::HyMt27BQ8;
    let (mut manager, blocking, _storage) = manager_behind_a_file(cx);
    cx.update(|cx| {
        manager.update(
            |config| {
                config.translation.model_directory = Some(directory.path().to_path_buf());
                config.translation.selected_model = Some(selected);
            },
            cx,
        );
    });
    let (_panel, cx) = settings_over(manager, Language::En, cx);
    let downloads = translation_downloads(cx);
    downloads.update(cx, |downloads, cx| {
        downloads.force_downloaded_for_test(selected, directory.path().to_path_buf(), cx);
        downloads.force_downloaded_for_test(model, directory.path().to_path_buf(), cx);
    });
    click(cx, "SETTINGS_ENTRY_ai.translation");
    settle(cx);

    assert_eq!(
        stored(cx).translation.selected_model,
        Some(selected),
        "the test starts with another downloaded model selected, which the page repair keeps"
    );

    click(cx, "SETTINGS_AI_MODEL_SELECT");
    click(cx, "TEST_MENU_ITEM_HY-MT2-1.8B-Q4_K_M");

    assert_eq!(
        stored(cx).translation.selected_model,
        Some(selected),
        "a selection that could not be stored must leave the stored selection as it was"
    );
    assert!(
        cx.debug_bounds("BASE_UI_ALERT").is_some(),
        "a selection that could not be stored should tell the user"
    );

    click(cx, "BASE_UI_ALERT_OK");
    std::fs::remove_file(&blocking).expect("the blocking file should be removable");

    click(cx, "SETTINGS_AI_MODEL_SELECT");
    click(cx, "TEST_MENU_ITEM_HY-MT2-1.8B-Q4_K_M");

    assert_eq!(
        stored(cx).translation.selected_model,
        Some(model),
        "the retry should store the model the user chose"
    );
}

//! The translation page: where models are stored, which one is used, and the models that can be
//! downloaded.
//!
//! The page is a view of its own rather than a page of fields: its model list is static content,
//! its directory is an input that only the system folder picker writes, and its download state
//! belongs to the process-wide [`crate::translation_downloads::TranslationDownloads`] rather than
//! to the settings session. The chosen directory and model are recorded in the session draft, so
//! confirming the dialog is what stores them, and the controls read them back from the draft, so
//! a session that is reopened shows the stored ones.

use std::path::{Path, PathBuf};

use app_config::translation::TranslationModel;
use base_ui::{
    button::Button,
    input::{Input, InputState, InputStateBuilder},
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    progress::Progress,
    select::Select,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use download_provider::error::DownloadFailure;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, base::Disableable as _, div,
};
use i18n::t;
use smol_str::format_smolstr;

use crate::{
    draft::{SettingsDraft, SettingsEdit},
    page,
    translation_downloads::{
        DeleteFailure, DownloadState, TranslationDownloads, TranslationDownloadsEvent,
        UnloadFailure,
    },
};

/// The element id of the directory field, which is also the key of its state.
const DIRECTORY_INPUT_ID: &str = "settings-ai-directory";

/// The translation page.
pub(crate) struct TranslationPage {
    draft: Entity<SettingsDraft>,
    /// The read-only field that shows the directory, built on the first frame that draws it.
    directory: Option<Entity<InputState>>,
    /// The process-wide downloads, when the composition root installed them.
    downloads: Option<Entity<TranslationDownloads>>,
    /// Held because a download that reports progress repaints this page through it.
    _observe: Option<Subscription>,
    /// Held because it is what turns a finished download into a fresh selection.
    _events: Option<Subscription>,
}

impl TranslationPage {
    pub(crate) fn new(
        draft: Entity<SettingsDraft>,
        downloads: Option<Entity<TranslationDownloads>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (observe, events) = match downloads.as_ref() {
            Some(downloads) => {
                let observe = cx.observe(downloads, |_, _, cx| cx.notify());
                let events = cx.subscribe(
                    downloads,
                    |page, _, event: &TranslationDownloadsEvent, cx| {
                        page.download_event(event.clone(), cx);
                    },
                );
                (Some(observe), Some(events))
            }
            None => (None, None),
        };
        let mut page = Self {
            draft,
            directory: None,
            downloads,
            _observe: observe,
            _events: events,
        };
        page.adopt_last_download_directory(cx);
        page.clear_selection_if_unavailable(cx);
        page
    }

    /// Shows the directory of the most recent completed download when this session was opened
    /// without one, so closing an unconfirmed session does not hide where its download went.
    ///
    /// A directory the session already holds is never replaced: the page is not allowed to
    /// change what a session is about to store just because a download ran elsewhere.
    fn adopt_last_download_directory(&mut self, cx: &mut Context<Self>) {
        if self
            .draft
            .read(cx)
            .config()
            .translation
            .model_directory
            .is_some()
        {
            return;
        }
        let Some(directory) = self.downloads.as_ref().and_then(|downloads| {
            downloads
                .read(cx)
                .last_download_directory()
                .map(Path::to_path_buf)
        }) else {
            return;
        };
        self.draft.update(cx, |draft, cx| {
            draft.write(SettingsEdit::TranslationModelDirectory { directory }, cx);
        });
    }

    /// Clears a stored selection whose file is not in the stored directory.
    fn clear_selection_if_unavailable(&mut self, cx: &mut Context<Self>) {
        let Some(downloads) = self.downloads.as_ref() else {
            return;
        };
        let config = self.draft.read(cx).config();
        let Some(selected) = config.translation.selected_model else {
            return;
        };
        let available = config
            .translation
            .model_directory
            .as_deref()
            .is_some_and(|directory| downloads.read(cx).is_downloaded(selected, directory));
        if !available {
            self.draft.update(cx, |draft, cx| {
                draft.write(SettingsEdit::TranslationModelSelected { model: None }, cx);
            });
        }
    }

    /// The read-only field that shows the directory the session holds.
    fn directory_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = self.directory.as_ref() {
            return input.clone();
        }

        let stored = self
            .draft
            .read(cx)
            .config()
            .translation
            .model_directory
            .clone();
        let value = stored.map_or_else(SharedString::default, |path| {
            SharedString::from(path.to_string_lossy().into_owned())
        });
        let input = cx.new(|cx| {
            InputStateBuilder::new(DIRECTORY_INPUT_ID)
                .default_value(value)
                .readonly()
                .build(window, cx)
        });
        self.directory = Some(input.clone());

        input
    }

    /// Asks the system for a directory and records the one chosen.
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = t!(cx, "settings_ui.ai.browse");
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt),
        });

        cx.spawn_in(window, async move |page, cx| {
            let Ok(Ok(Some(paths))) = answer.await else {
                return;
            };
            let Some(directory) = paths.into_iter().next() else {
                return;
            };
            let _ = page.update_in(cx, |page, window, cx| page.choose(directory, window, cx));
        })
        .detach();
    }

    /// Records `directory` as the session's model directory and shows it in the field.
    fn choose(&mut self, directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let unchanged = {
            let config = self.draft.read(cx).config();
            config.translation.model_directory.as_deref() == Some(directory.as_path())
        };
        if unchanged {
            return;
        }

        let shown = SharedString::from(directory.to_string_lossy().into_owned());
        if let Some(input) = self.directory.as_ref() {
            input.update(cx, |input, cx| input.update_value(&shown, window, cx));
        }
        self.draft.update(cx, |draft, cx| {
            draft.write(
                SettingsEdit::TranslationModelDirectory {
                    directory: directory.clone(),
                },
                cx,
            );
        });

        let selected = self.draft.read(cx).config().translation.selected_model;
        if let Some(selected) = selected {
            let available = self
                .downloads
                .as_ref()
                .is_some_and(|downloads| downloads.read(cx).is_downloaded(selected, &directory));
            if !available {
                self.draft.update(cx, |draft, cx| {
                    draft.write(SettingsEdit::TranslationModelSelected { model: None }, cx);
                });
            }
        }
        cx.notify();
    }

    /// Answers a download or a delete the manager reports.
    fn download_event(&mut self, event: TranslationDownloadsEvent, cx: &mut Context<Self>) {
        match event {
            TranslationDownloadsEvent::Completed { model, directory } => {
                let config = self.draft.read(cx).config();
                let same_directory =
                    config.translation.model_directory.as_ref() == Some(&directory);
                let available = self.downloads.as_ref().is_some_and(|downloads| {
                    config.translation.selected_model.is_some_and(|selected| {
                        downloads.read(cx).is_downloaded(selected, &directory)
                    })
                });
                if same_directory && !available {
                    self.draft.update(cx, |draft, cx| {
                        draft.write(
                            SettingsEdit::TranslationModelSelected { model: Some(model) },
                            cx,
                        );
                    });
                }
            }
            TranslationDownloadsEvent::Deleted { model, .. } => {
                if self.draft.read(cx).config().translation.selected_model == Some(model) {
                    self.draft.update(cx, |draft, cx| {
                        draft.write(SettingsEdit::TranslationModelSelected { model: None }, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    /// The directory the field shows, for the dialog's tests.
    #[cfg(test)]
    pub(crate) fn shown_directory(&self, cx: &gpui_kit::App) -> Option<SharedString> {
        self.directory
            .as_ref()
            .map(|input| input.read(cx).value(cx))
    }

    /// Downloading needs both a directory and the process-wide download manager.
    fn download_disabled(&self, directory: Option<&Path>) -> bool {
        self.downloads.is_none() || directory.is_none()
    }

    /// The button that deletes a model, which also stops the session naming it.
    fn delete_button(
        &self,
        index: usize,
        model: TranslationModel,
        directory: Option<&Path>,
        cx: &mut Context<Self>,
    ) -> Button {
        let downloads = self.downloads.clone();
        let draft = self.draft.clone();
        let download_directory = directory.map(Path::to_path_buf);

        Button::new(model_element_id("settings-ai-delete", index))
            .tonal()
            .debug_selector(move || format!("SETTINGS_AI_MODEL_DELETE_{index}"))
            .aria_label(t!(cx, "settings_ui.ai.delete"))
            .child(t!(cx, "settings_ui.ai.delete"))
            .on_click(cx.listener({
                move |_, _, _, cx| {
                    let (Some(downloads), Some(directory)) =
                        (downloads.as_ref(), download_directory.as_ref())
                    else {
                        return;
                    };
                    // The session stops naming the model only when it was the one this
                    // directory held: deleting another model is not a change of choice.
                    let clears_selection = {
                        let config = draft.read(cx).config();
                        config.translation.selected_model == Some(model)
                            && config.translation.model_directory.as_deref()
                                == Some(directory.as_path())
                    };
                    if clears_selection {
                        draft.update(cx, |draft, cx| {
                            draft.write(SettingsEdit::TranslationModelSelected { model: None }, cx);
                        });
                    }
                    downloads.update(cx, |downloads, cx| {
                        downloads.delete(model, directory.clone(), cx);
                    });
                }
            }))
    }

    /// One model's row: what it is, where it comes from, and what can be done with it.
    fn model_row(
        &self,
        index: usize,
        model: TranslationModel,
        directory: Option<&Path>,
        is_downloaded: bool,
        state: DownloadState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let model_name = format_smolstr!(
            "{} {}",
            model.display_name(),
            t!(
                cx,
                "settings_ui.ai.model_size",
                size = format!("{:.2}", model.size_mb())
            )
        );
        let download_directory = directory.map(Path::to_path_buf);
        let downloads = self.downloads.clone();
        let percent_of = |downloaded: u64| {
            if model.size_bytes() == 0 {
                0.0
            } else {
                (downloaded as f32 / model.size_bytes() as f32 * 100.0).clamp(0.0, 100.0)
            }
        };

        let mut action = div().flex_none();
        let mut detail: Option<AnyElement> = None;

        if matches!(&state, DownloadState::Deleting) {
            action = action.child(
                Button::new(model_element_id("settings-ai-deleting", index))
                    .tonal()
                    .disabled(true)
                    .debug_selector(move || format!("SETTINGS_AI_MODEL_DELETING_{index}"))
                    .aria_label(t!(cx, "settings_ui.ai.deleting"))
                    .child(t!(cx, "settings_ui.ai.deleting")),
            );
        } else if is_downloaded {
            if let DownloadState::DeleteFailed { failure } = &state {
                let reason = delete_failure_reason(failure, cx);
                detail = Some(
                    div()
                        .debug_selector(move || format!("SETTINGS_AI_MODEL_ERROR_{index}"))
                        .text_xs()
                        .text_color(cx.theme().roles.fg.muted.hsla)
                        .child(t!(cx, "settings_ui.ai.delete_failed", reason = reason))
                        .into_any_element(),
                );
            }
            action = action.child(self.delete_button(index, model, directory, cx));
        } else if let DownloadState::Downloading { downloaded } = &state {
            let percent = percent_of(*downloaded);
            detail = Some(download_progress(
                index,
                percent,
                t!(
                    cx,
                    "settings_ui.ai.progress",
                    percent = format!("{percent:.0}")
                ),
                cx,
            ));
            action = action.child(
                Button::new(model_element_id("settings-ai-pause", index))
                    .tonal()
                    .debug_selector(move || format!("SETTINGS_AI_MODEL_PAUSE_{index}"))
                    .aria_label(t!(cx, "settings_ui.ai.pause"))
                    .child(t!(cx, "settings_ui.ai.pause"))
                    .on_click(cx.listener({
                        move |_, _, _, cx| {
                            let (Some(downloads), Some(directory)) =
                                (downloads.as_ref(), download_directory.as_ref())
                            else {
                                return;
                            };
                            downloads.update(cx, |downloads, cx| {
                                downloads.pause(model, directory.clone(), cx);
                            });
                        }
                    })),
            );
        } else if let DownloadState::Pausing { downloaded } = &state {
            detail = Some(download_progress(
                index,
                percent_of(*downloaded),
                t!(cx, "settings_ui.ai.pausing"),
                cx,
            ));
            action = action.child(
                Button::new(model_element_id("settings-ai-pausing", index))
                    .tonal()
                    .disabled(true)
                    .debug_selector(move || format!("SETTINGS_AI_MODEL_PAUSING_{index}"))
                    .aria_label(t!(cx, "settings_ui.ai.pausing"))
                    .child(t!(cx, "settings_ui.ai.pausing")),
            );
        } else if let DownloadState::Paused { downloaded } = &state {
            detail = Some(download_progress(
                index,
                percent_of(*downloaded),
                t!(cx, "settings_ui.ai.paused"),
                cx,
            ));
            action = action
                .child(
                    Button::new(model_element_id("settings-ai-resume", index))
                        .tonal()
                        .debug_selector(move || format!("SETTINGS_AI_MODEL_RESUME_{index}"))
                        .aria_label(t!(cx, "settings_ui.ai.resume"))
                        .child(t!(cx, "settings_ui.ai.resume"))
                        .on_click(cx.listener({
                            move |_, _, _, cx| {
                                let (Some(downloads), Some(directory)) =
                                    (downloads.as_ref(), download_directory.as_ref())
                                else {
                                    return;
                                };
                                downloads.update(cx, |downloads, cx| {
                                    downloads.resume(model, directory.clone(), cx);
                                });
                            }
                        })),
                )
                .child(self.delete_button(index, model, directory, cx));
        } else if let DownloadState::DownloadFailed { failure } = &state {
            let reason = download_failure_reason(failure, cx);
            detail = Some(
                div()
                    .debug_selector(move || format!("SETTINGS_AI_MODEL_ERROR_{index}"))
                    .text_xs()
                    .text_color(cx.theme().roles.fg.muted.hsla)
                    .child(t!(cx, "settings_ui.ai.download_failed", reason = reason))
                    .into_any_element(),
            );
            // A failed download can be discarded like a paused one: its partial file is
            // otherwise left for a retry the user may not want.
            action = action
                .child(self.delete_button(index, model, directory, cx))
                .child(
                    Button::new(model_element_id("settings-ai-retry", index))
                        .tonal()
                        .debug_selector(move || format!("SETTINGS_AI_MODEL_RETRY_{index}"))
                        .aria_label(t!(cx, "settings_ui.ai.retry"))
                        .child(t!(cx, "settings_ui.ai.retry"))
                        .on_click(cx.listener({
                            move |_, _, _, cx| {
                                let (Some(downloads), Some(directory)) =
                                    (downloads.as_ref(), download_directory.as_ref())
                                else {
                                    return;
                                };
                                downloads.update(cx, |downloads, cx| {
                                    downloads.download(model, directory.clone(), cx);
                                });
                            }
                        })),
                );
        } else {
            if let DownloadState::DeleteFailed { failure } = &state {
                let reason = delete_failure_reason(failure, cx);
                detail = Some(
                    div()
                        .debug_selector(move || format!("SETTINGS_AI_MODEL_ERROR_{index}"))
                        .text_xs()
                        .text_color(cx.theme().roles.fg.muted.hsla)
                        .child(t!(cx, "settings_ui.ai.delete_failed", reason = reason))
                        .into_any_element(),
                );
            }
            let disabled = self.download_disabled(directory);
            action = action.child(
                Button::new(model_element_id("settings-ai-download", index))
                    .tonal()
                    .disabled(disabled)
                    .debug_selector(move || format!("SETTINGS_AI_MODEL_DOWNLOAD_{index}"))
                    .aria_label(t!(cx, "settings_ui.ai.download"))
                    .child(t!(cx, "settings_ui.ai.download"))
                    .on_click(cx.listener({
                        move |_, _, _, cx| {
                            let (Some(downloads), Some(directory)) =
                                (downloads.as_ref(), download_directory.as_ref())
                            else {
                                return;
                            };
                            downloads.update(cx, |downloads, cx| {
                                downloads.download(model, directory.clone(), cx);
                            });
                        }
                    })),
            );
        }

        let mut row = div()
            .debug_selector(move || format!("SETTINGS_AI_MODEL_{index}"))
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().roles.fg.default.hsla)
                                    .child(SharedString::from(model_name.as_str())),
                            )
                            .child(
                                div()
                                    .debug_selector(move || {
                                        format!("SETTINGS_AI_MODEL_URL_{index}")
                                    })
                                    .text_xs()
                                    .text_color(cx.theme().roles.fg.muted.hsla)
                                    .child(t!(
                                        cx,
                                        "settings_ui.ai.download_url",
                                        url = model.download_url()
                                    )),
                            ),
                    )
                    .child(action),
            );
        if let Some(detail) = detail {
            row = row.child(detail);
        }

        row.into_any_element()
    }
}

impl Render for TranslationPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = self.directory_input(window, cx);
        let config = self.draft.read(cx).config();
        let directory = config.translation.model_directory.clone();
        let downloaded = match (self.downloads.as_ref(), directory.as_deref()) {
            (Some(downloads), Some(directory)) => downloads.read(cx).downloaded_models(directory),
            _ => Vec::new(),
        };
        let selected = config
            .translation
            .selected_model
            .filter(|model| downloaded.contains(model));
        let disabled = directory.is_none() || downloaded.is_empty();
        let placeholder = if directory.is_some() {
            t!(cx, "settings_ui.ai.no_downloaded_models")
        } else {
            t!(cx, "settings_ui.ai.no_model_directory")
        };

        let mut select = Select::new("settings-ai-model-select")
            .tonal()
            .placeholder(placeholder)
            .disabled(disabled)
            .aria_label(t!(cx, "settings_ui.ai.model_in_use"));
        for model in &downloaded {
            select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                model.display_name(),
                model.display_name(),
            )));
        }
        let page = cx.entity().downgrade();
        let select = select
            .selected_item(selected.map(|model| SharedString::from(model.display_name())))
            .on_change(move |item_id, _, cx| {
                let Some(model) = item_id
                    .as_ref()
                    .and_then(|id| TranslationModel::from_name(id.as_str()))
                else {
                    return;
                };
                if let Some(page) = page.upgrade() {
                    page.update(cx, |page, cx| {
                        page.draft.update(cx, |draft, cx| {
                            draft.write(
                                SettingsEdit::TranslationModelSelected { model: Some(model) },
                                cx,
                            );
                        });
                        cx.notify();
                    });
                }
            });

        let mut models = div().w_full().flex().flex_col().gap_2();
        for (index, model) in TranslationModel::ALL.into_iter().enumerate() {
            let state = self
                .downloads
                .as_ref()
                .map_or(DownloadState::Idle, |downloads| {
                    directory
                        .as_deref()
                        .map_or(DownloadState::Idle, |directory| {
                            downloads.read(cx).row_state(model, directory)
                        })
                });
            let is_downloaded = self.downloads.as_ref().is_some_and(|downloads| {
                directory
                    .as_deref()
                    .is_some_and(|directory| downloads.read(cx).is_downloaded(model, directory))
            });
            models = models.child(self.model_row(
                index,
                model,
                directory.as_deref(),
                is_downloaded,
                state,
                cx,
            ));
        }

        div()
            .debug_selector(|| "SETTINGS_AI_PAGE".into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_4()
            .py_1()
            .child(
                div()
                    .debug_selector(|| "SETTINGS_AI_DIRECTORY".into())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(page::section_title(
                        t!(cx, "settings_ui.ai.model_directory"),
                        cx,
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .debug_selector(|| "SETTINGS_AI_DIRECTORY_INPUT".into())
                                    .child(Input::new(&input).medium()),
                            )
                            .child(
                                Button::new("settings-ai-browse")
                                    .tonal()
                                    .debug_selector(|| "SETTINGS_AI_BROWSE".into())
                                    .aria_label(t!(cx, "settings_ui.ai.browse"))
                                    .child(t!(cx, "settings_ui.ai.browse"))
                                    .on_click(cx.listener(|page, _, window, cx| {
                                        page.browse(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "SETTINGS_AI_MODEL_SELECT_SECTION".into())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(page::section_title(
                        t!(cx, "settings_ui.ai.model_in_use"),
                        cx,
                    ))
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .debug_selector(|| "SETTINGS_AI_MODEL_SELECT".into())
                            .child(select),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "SETTINGS_AI_MODELS".into())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(page::section_title(t!(cx, "settings_ui.ai.models"), cx))
                    .child(models),
            )
    }
}

/// The element id of one control of one model row.
fn model_element_id(prefix: &'static str, index: usize) -> ElementId {
    ElementId::NamedInteger(SharedString::new_static(prefix), index as u64)
}

/// The progress bar and its caption of one model row.
fn download_progress(index: usize, percent: f32, caption: SharedString, cx: &App) -> AnyElement {
    div()
        .debug_selector(move || format!("SETTINGS_AI_MODEL_PROGRESS_{index}"))
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(Progress::new(model_element_id("settings-ai-progress", index)).value(percent))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(caption),
        )
        .into_any_element()
}

/// The text a download failure is drawn with, in the interface's language.
fn download_failure_reason(failure: &DownloadFailure, cx: &App) -> SharedString {
    match failure {
        DownloadFailure::Status { status } => {
            t!(cx, "settings_ui.ai.download_failed_status", status = status)
        }
        // The transport that could not be started, the request that could not be sent and the
        // body that could not be read are one message to the reader: the download failed on
        // the network.
        DownloadFailure::Transport | DownloadFailure::Request | DownloadFailure::Read => {
            t!(cx, "settings_ui.ai.download_failed_transport")
        }
        DownloadFailure::Route => t!(cx, "settings_ui.ai.download_failed_route"),
        DownloadFailure::Range => t!(cx, "settings_ui.ai.download_failed_range"),
        DownloadFailure::SizeMismatch { expected, actual } => t!(
            cx,
            "settings_ui.ai.download_failed_size",
            expected = expected,
            actual = actual
        ),
        DownloadFailure::TooLarge { .. } => t!(cx, "settings_ui.ai.download_failed_too_large"),
        DownloadFailure::Cancelled => t!(cx, "settings_ui.ai.download_failed_cancelled"),
        DownloadFailure::Io { .. } => t!(cx, "settings_ui.ai.download_failed_io"),
        DownloadFailure::InvalidUrl => t!(cx, "settings_ui.ai.download_failed_url"),
        DownloadFailure::InvalidDestination => t!(cx, "settings_ui.ai.download_failed_destination"),
        DownloadFailure::Redirect => t!(cx, "settings_ui.ai.download_failed_redirect"),
        DownloadFailure::Task => t!(cx, "settings_ui.ai.download_failed_task"),
    }
}

/// The text a delete failure is drawn with, in the interface's language.
fn delete_failure_reason(failure: &DeleteFailure, cx: &App) -> SharedString {
    match failure {
        DeleteFailure::Unload(UnloadFailure::TimedOut) => {
            t!(cx, "settings_ui.ai.delete_failed_unload_timeout")
        }
        DeleteFailure::Unload(UnloadFailure::Engine) => {
            t!(cx, "settings_ui.ai.delete_failed_unload")
        }
        DeleteFailure::Io { .. } => t!(cx, "settings_ui.ai.delete_failed_io"),
        DeleteFailure::TimedOut => t!(cx, "settings_ui.ai.delete_failed_timeout"),
        DeleteFailure::Provider(_) => t!(cx, "settings_ui.ai.delete_failed_provider"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::{settings::Config, translation::TranslationModel};
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use network::transport::hub::{TransportHub, TransportPolicy};
    use tempfile::TempDir;
    use widget_tokio_runtime::runtime::Runtime;

    use super::TranslationPage;
    use crate::{
        draft::SettingsDraft,
        translation_downloads::{DownloadState, TranslationDownloads, TranslationDownloadsDeps},
    };

    fn downloads(cx: &mut TestAppContext) -> Entity<TranslationDownloads> {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
            let provider = download_provider::global::install(
                cx,
                download_provider::global::DownloadProviderDeps {
                    transport: Arc::new(TransportHub::new(TransportPolicy::default())),
                    runtime: Runtime::for_test(),
                },
            );
            cx.new(|cx| {
                TranslationDownloads::new(
                    TranslationDownloadsDeps {
                        provider,
                        runtime: Runtime::for_test(),
                        unload: Arc::new(|_: &std::path::Path| Ok(())),
                    },
                    cx,
                )
            })
        })
    }

    /// Without the process-wide manager there is nothing a download button could start.
    #[gpui_kit::test]
    fn download_buttons_should_be_disabled_without_downloads(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let draft = cx.update(|cx| cx.new(|_| SettingsDraft::new(Config::default())));
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(draft, None, cx)));

        assert!(cx.read(|cx| page.read(cx).download_disabled(Some(directory.path()))));
        assert!(cx.read(|cx| page.read(cx).download_disabled(None)));
    }

    /// A directory the session already holds is not replaced by the last download's one, even
    /// when a download is running in that other directory.
    #[gpui_kit::test]
    fn a_page_should_keep_the_directory_its_draft_already_has(cx: &mut TestAppContext) {
        let chosen = TempDir::new().expect("the chosen directory builds");
        let downloaded = TempDir::new().expect("the download directory builds");
        let downloads = downloads(cx);
        downloads.update(cx, |downloads, cx| {
            downloads.set_last_download_directory_for_test(downloaded.path().to_path_buf());
            downloads.set_state_for_test(
                TranslationModel::HyMt218BQ4Km,
                downloaded.path().to_path_buf(),
                DownloadState::Downloading { downloaded: 42 },
                cx,
            );
        });
        let mut config = Config::default();
        config.translation.model_directory = Some(chosen.path().to_path_buf());
        let draft = cx.update(|cx| cx.new(|_| SettingsDraft::new(config)));

        let _page =
            cx.update(|cx| cx.new(|cx| TranslationPage::new(draft.clone(), Some(downloads), cx)));

        assert_eq!(
            cx.read(|cx| draft.read(cx).config().translation.model_directory.clone()),
            Some(chosen.path().to_path_buf()),
            "the page must not rewrite a directory the session already holds"
        );
    }

    /// A session opened without a directory shows the one the last download completed in.
    #[gpui_kit::test]
    fn a_page_should_adopt_the_last_download_directory_when_it_has_none(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let downloads = downloads(cx);
        downloads.update(cx, |downloads, cx| {
            downloads.complete_download_for_test(
                TranslationModel::HyMt27BQ8,
                directory.path().to_path_buf(),
                cx,
            );
        });
        let draft = cx.update(|cx| cx.new(|_| SettingsDraft::new(Config::default())));

        let _page =
            cx.update(|cx| cx.new(|cx| TranslationPage::new(draft.clone(), Some(downloads), cx)));

        let config = cx.read(|cx| draft.read(cx).config().as_ref().clone());
        assert_eq!(
            config.translation.model_directory,
            Some(directory.path().to_path_buf()),
            "a session with no directory should open on the one the download finished in"
        );
        assert_eq!(
            config.translation.selected_model, None,
            "opening the page must not select a model the user never chose"
        );
    }
}

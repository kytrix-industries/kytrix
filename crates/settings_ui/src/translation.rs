//! The translation page: where models are stored, which one is used, and the models that can be
//! downloaded.
//!
//! The page is a view of its own rather than a page of fields: its model list is a table of its
//! own, its directory is an input that only the system folder picker writes, and its download
//! state belongs to the process-wide [`crate::translation_downloads::TranslationDownloads`]
//! rather than to the settings session. The chosen directory and model are recorded in the
//! session draft, so confirming the dialog is what stores them, and the controls read them back
//! from the draft, so a session that is reopened shows the stored ones.
//!
//! Downloading starts from the site the session holds: the two sites are drawn under the
//! directory, and both a first download and a retry of a failed one use the chosen site.

use std::path::{Path, PathBuf};

use app_config::translation::{DownloadSite, TranslationModel};
use base_ui::{
    button::Button,
    input::{Input, InputState, InputStateBuilder},
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    toggle::Toggle,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement,
    PathPromptOptions, Render, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Window, base::Disableable as _, div,
};
use i18n::t;

use crate::{
    draft::{SettingsDraft, SettingsEdit},
    page,
    translation_downloads::{TranslationDownloads, TranslationDownloadsEvent},
    translation_table::{TranslationRequest, TranslationTable},
};

/// The element id of the directory field, which is also the key of its state.
const DIRECTORY_INPUT_ID: &str = "settings-ai-directory";

/// What one site is called in the interface.
fn site_label(site: DownloadSite, cx: &App) -> SharedString {
    match site {
        DownloadSite::HuggingFace => t!(cx, "settings_ui.ai.download_site_huggingface"),
        DownloadSite::ModelScope => t!(cx, "settings_ui.ai.download_site_modelscope"),
    }
}

/// What one site's radio is drawn under, which is what a test chooses it by.
fn site_selector(site: DownloadSite) -> &'static str {
    match site {
        DownloadSite::HuggingFace => "SETTINGS_AI_SITE_HUGGINGFACE",
        DownloadSite::ModelScope => "SETTINGS_AI_SITE_MODELSCOPE",
    }
}

/// The translation page.
pub(crate) struct TranslationPage {
    draft: Entity<SettingsDraft>,
    /// The read-only field that shows the directory, built on the first frame that draws it.
    directory: Option<Entity<InputState>>,
    /// The process-wide downloads, when the composition root installed them.
    downloads: Option<Entity<TranslationDownloads>>,
    /// The supported models and what each of them is doing.
    table: Entity<TranslationTable>,
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
        let table = cx.new(|cx| TranslationTable::new(draft.clone(), downloads.clone(), cx));
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
            table,
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

    /// Takes the action a row asked for and answers it, before the page draws that action's
    /// result.
    fn handle_request(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.table.update(cx, |table, _| table.take_request()) else {
            return;
        };
        let directory = self
            .draft
            .read(cx)
            .config()
            .translation
            .model_directory
            .clone();

        match request {
            TranslationRequest::Download { model } => {
                let site = self.draft.read(cx).config().translation.download_site;
                self.start_model_download(model, site, false, cx);
            }
            TranslationRequest::Retry { model } => {
                let site = self.draft.read(cx).config().translation.download_site;
                self.start_model_download(model, site, true, cx);
            }
            TranslationRequest::Resume { model } => {
                let (Some(directory), Some(downloads)) = (directory, self.downloads.clone()) else {
                    return;
                };
                downloads.update(cx, |downloads, cx| {
                    downloads.resume(model, directory, cx);
                });
            }
            TranslationRequest::Terminate { model } => {
                let (Some(directory), Some(downloads)) = (directory, self.downloads.clone()) else {
                    return;
                };
                self.clear_selection(model, &directory, cx);
                downloads.update(cx, |downloads, cx| {
                    downloads.terminate(model, directory, cx);
                });
            }
            TranslationRequest::Delete { model } => {
                let (Some(directory), Some(downloads)) = (directory, self.downloads.clone()) else {
                    return;
                };
                self.clear_selection(model, &directory, cx);
                downloads.update(cx, |downloads, cx| {
                    downloads.delete(model, directory, cx);
                });
            }
        }
    }

    /// Stops the session naming `model` when it was the model `directory` held.
    fn clear_selection(
        &mut self,
        model: TranslationModel,
        directory: &Path,
        cx: &mut Context<Self>,
    ) {
        let clears_selection = {
            let config = self.draft.read(cx).config();
            config.translation.selected_model == Some(model)
                && config.translation.model_directory.as_deref() == Some(directory)
        };
        if clears_selection {
            self.draft.update(cx, |draft, cx| {
                draft.write(SettingsEdit::TranslationModelSelected { model: None }, cx);
            });
        }
    }

    /// Starts the download of `model` from `site`.
    fn start_model_download(
        &mut self,
        model: TranslationModel,
        site: DownloadSite,
        retry: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = self
            .draft
            .read(cx)
            .config()
            .translation
            .model_directory
            .clone()
        else {
            return;
        };
        let Some(downloads) = self.downloads.clone() else {
            return;
        };

        downloads.update(cx, |downloads, cx| {
            if retry {
                downloads.retry(model, directory, site, cx);
            } else {
                downloads.download(model, directory, site, cx);
            }
        });
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
}

impl Render for TranslationPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.handle_request(cx);

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

        let download_site = config.translation.download_site;
        let site_page = cx.entity().downgrade();
        let site_choices = DownloadSite::ALL.into_iter().fold(
            div().w_full().flex().flex_col().gap_1(),
            |choices, site| {
                let page = site_page.clone();
                choices.child(
                    Toggle::new(site_selector(site))
                        .radio()
                        .checked(download_site == site)
                        .label(site_label(site, cx))
                        .debug_selector(move || site_selector(site).into())
                        .on_change(move |checked, _, cx| {
                            if !checked {
                                return;
                            }
                            let Some(page) = page.upgrade() else {
                                return;
                            };
                            page.update(cx, |page, cx| {
                                page.draft.update(cx, |draft, cx| {
                                    draft.write(SettingsEdit::TranslationDownloadSite { site }, cx);
                                });
                                cx.notify();
                            });
                        }),
                )
            },
        );

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
                    .flex_none()
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
                    .debug_selector(|| "SETTINGS_AI_DOWNLOAD_SITE".into())
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(page::section_title(
                        t!(cx, "settings_ui.ai.download_site"),
                        cx,
                    ))
                    .child(site_choices),
            )
            .child(
                div()
                    .debug_selector(|| "SETTINGS_AI_MODEL_SELECT_SECTION".into())
                    .flex_none()
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
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(page::section_title(t!(cx, "settings_ui.ai.models"), cx))
                    .child(self.table.clone()),
            )
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

    /// The table offers a download only when both the process-wide manager and a directory
    /// exist: without either there is nothing a download could start.
    #[gpui_kit::test]
    fn the_table_should_offer_no_download_without_a_manager_or_a_directory(
        cx: &mut TestAppContext,
    ) {
        let directory = TempDir::new().expect("the test directory builds");
        let draft = cx.update(|cx| cx.new(|_| SettingsDraft::new(Config::default())));
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(draft, None, cx)));

        assert!(
            !cx.read(|cx| page.read(cx).table.read(cx).downloads_enabled()),
            "a page without the process-wide manager cannot download"
        );

        let downloads = downloads(cx);
        let draft = cx.update(|cx| cx.new(|_| SettingsDraft::new(Config::default())));
        let page =
            cx.update(|cx| cx.new(|cx| TranslationPage::new(draft, Some(downloads.clone()), cx)));

        assert!(
            !cx.read(|cx| page.read(cx).table.read(cx).downloads_enabled()),
            "a session with no directory cannot download"
        );

        let draft = cx.update(|cx| {
            cx.new(|_| {
                let mut config = Config::default();
                config.translation.model_directory = Some(directory.path().to_path_buf());
                SettingsDraft::new(config)
            })
        });
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(draft, Some(downloads), cx)));

        assert!(
            cx.read(|cx| page.read(cx).table.read(cx).downloads_enabled()),
            "the manager and a directory are what the download action needs"
        );
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
                DownloadState::Downloading {
                    downloaded: 42,
                    mb_per_second: 2.0,
                },
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

//! The translation page: where models are stored, which one is used, and the models that can be
//! downloaded.
//!
//! The page is a view of its own rather than a page of fields: its model list is a table of its
//! own, its directory is an input that only the system folder picker writes, and its download
//! state belongs to the process-wide [`crate::translation_downloads::TranslationDownloads`]
//! rather than to the settings page. The chosen directory and model are written into the settings
//! as they are chosen, and the controls read them back from there, so what the page draws is what
//! is stored.
//!
//! Two repairs are made the first time the page is really drawn, and only then — see
//! [`TranslationPage::repair_on_open`]: a directory is adopted from the last completed download
//! when none is stored, and a selection that the stored directory does not hold is cleared. They
//! are written together, in one write, so a page opened without being entered writes nothing.
//!
//! Downloading starts from the site the settings hold: the two sites are drawn under the
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
    page,
    session::{SettingsChange, SettingsSession},
    translation_downloads::TranslationDownloads,
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
    session: Entity<SettingsSession>,
    /// The read-only field that shows the directory, built on the first frame that draws it.
    directory: Option<Entity<InputState>>,
    /// The process-wide downloads, when the composition root installed them.
    downloads: Option<Entity<TranslationDownloads>>,
    /// The supported models and what each of them is doing.
    table: Entity<TranslationTable>,
    /// Whether the repairs of [`Self::repair_on_open`] have been scheduled, so they happen once.
    repair_started: bool,
    /// Held because a write of the settings can land here rather than through this page — the
    /// repairs the page schedules are written through the session — and the field, the site
    /// radios and the model select all draw what the settings hold.
    _session: Subscription,
    /// Held because every change the download manager publishes repaints this page through it:
    /// it selects a finished download and clears the selection of a model it starts removing
    /// through the settings, and notifies on each of those changes.
    _observe: Option<Subscription>,
}

impl TranslationPage {
    pub(crate) fn new(
        session: Entity<SettingsSession>,
        downloads: Option<Entity<TranslationDownloads>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| TranslationTable::new(session.clone(), downloads.clone(), cx));
        let session_changes = cx.observe(&session, |_, _, cx| cx.notify());
        let observe = downloads
            .as_ref()
            .map(|downloads| cx.observe(downloads, |_, _, cx| cx.notify()));

        Self {
            session,
            directory: None,
            downloads,
            table,
            repair_started: false,
            _session: session_changes,
            _observe: observe,
        }
    }

    /// Schedules the two repairs the page makes, once, when it is first opened.
    ///
    /// A settings page that is opened but never shows this page must not write the translation
    /// settings, so nothing is written when the page is built: the repairs are scheduled here,
    /// when the page is moved to, and run at the end of the effect cycle that moved there.
    pub(crate) fn repair_on_open(&mut self, cx: &mut Context<Self>) {
        if self.repair_started {
            return;
        }
        self.repair_started = true;

        let session = self.session.clone();
        let downloads = self.downloads.clone();
        cx.defer(move |cx| Self::repair(session, downloads, cx));
    }

    /// Adopts the directory of the most recent completed download when none is stored, and clears
    /// a selection the stored directory does not hold.
    ///
    /// The two are one write: the directory a download finished in is the directory the selection
    /// is judged against, so storing one without the other would leave a selection naming a
    /// directory the settings no longer have. A directory that is already stored is never
    /// replaced — a download that ran elsewhere is not a reason to move the user's setting.
    ///
    /// A model is available when its file is in that directory at its published size and its row
    /// is not being removed: a file that is still there while the engine unloads is not one the
    /// page can load.
    fn repair(
        session: Entity<SettingsSession>,
        downloads: Option<Entity<TranslationDownloads>>,
        cx: &mut App,
    ) {
        let Some(downloads) = downloads else {
            return;
        };

        let config = session.read(cx).config(cx);
        let stored = config.translation.model_directory.clone();
        let adopted = stored.clone().or_else(|| {
            downloads
                .read(cx)
                .last_download_directory()
                .map(Path::to_path_buf)
        });

        let mut changes = Vec::with_capacity(2);
        if stored.is_none()
            && let Some(directory) = adopted.clone()
        {
            changes.push(SettingsChange::TranslationModelDirectory { directory });
        }
        if let Some(selected) = config.translation.selected_model {
            let available = adopted.as_deref().is_some_and(|directory| {
                let downloads = downloads.read(cx);
                !downloads.row_state(selected, directory).is_removing()
                    && downloads.is_downloaded(selected, directory)
            });
            if !available {
                changes.push(SettingsChange::TranslationModelSelected { model: None });
            }
        }
        drop(config);

        if changes.is_empty() {
            return;
        }
        session.update(cx, |session, cx| {
            let _ = session.write_all(changes, cx);
        });
    }

    /// The read-only field that shows the directory the settings hold.
    fn directory_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let stored = self
            .session
            .read(cx)
            .config(cx)
            .translation
            .model_directory
            .clone();
        let value = stored.map_or_else(SharedString::default, |path| {
            SharedString::from(path.to_string_lossy().into_owned())
        });

        if let Some(input) = self.directory.as_ref() {
            // The field is readonly, so its value only moves when the settings do — a repair
            // that adopted a directory, or a directory written elsewhere. What it shows is what
            // is stored, exactly as the other controls do.
            if input.read(cx).value(cx) != value {
                input.update(cx, |input, cx| input.update_value(&value, window, cx));
            }

            return input.clone();
        }

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

    /// Writes `directory` as the model directory and shows it in the field.
    ///
    /// A selection that the new directory cannot load is cleared in the same write: the menu is
    /// drawn from what the stored directory holds, so a directory stored without that clearing
    /// would be one frame away from naming a model that is not in it. The field shows the
    /// directory only once it is stored — a write that failed leaves the old one on screen, and
    /// the settings page reports why.
    fn choose(&mut self, directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let (unchanged, clears_selection) = {
            let config = self.session.read(cx).config(cx);
            let unchanged =
                config.translation.model_directory.as_deref() == Some(directory.as_path());
            let clears_selection = config.translation.selected_model.is_some_and(|selected| {
                !self.downloads.as_ref().is_some_and(|downloads| {
                    let downloads = downloads.read(cx);
                    !downloads.row_state(selected, &directory).is_removing()
                        && downloads.is_downloaded(selected, &directory)
                })
            });

            (unchanged, clears_selection)
        };
        if unchanged {
            return;
        }

        let mut changes = Vec::with_capacity(2);
        changes.push(SettingsChange::TranslationModelDirectory {
            directory: directory.clone(),
        });
        if clears_selection {
            changes.push(SettingsChange::TranslationModelSelected { model: None });
        }

        let stored = self
            .session
            .update(cx, |session, cx| session.write_all(changes, cx));
        if stored.is_err() {
            return;
        }

        let shown = SharedString::from(directory.to_string_lossy().into_owned());
        if let Some(input) = self.directory.as_ref() {
            input.update(cx, |input, cx| input.update_value(&shown, window, cx));
        }
        cx.notify();
    }

    /// Writes `model` as the model the local translation is loaded from, or clears the selection
    /// when the stored directory cannot load it.
    ///
    /// The menu only offers what the directory holds and is not being removed, so what this
    /// refuses is a model that stopped being usable between the frame the menu was drawn in and
    /// the choice — a delete that started while the menu was open. Clearing rather than writing
    /// it is what keeps the settings from naming a file that is on its way out.
    fn select_model(&mut self, model: TranslationModel, cx: &mut Context<Self>) {
        let available = {
            let config = self.session.read(cx).config(cx);
            config
                .translation
                .model_directory
                .as_deref()
                .is_some_and(|directory| {
                    self.downloads.as_ref().is_some_and(|downloads| {
                        let downloads = downloads.read(cx);
                        !downloads.row_state(model, directory).is_removing()
                            && downloads.is_downloaded(model, directory)
                    })
                })
        };

        let selected = available.then_some(model);
        self.session.update(cx, |session, cx| {
            let change = SettingsChange::TranslationModelSelected { model: selected };
            let _ = session.write(change, cx);
        });
        cx.notify();
    }

    /// Takes the action a row asked for and answers it, before the page draws that action's
    /// result.
    ///
    /// Stopping and deleting a model are the download manager's own: it clears a selection that
    /// names the model it starts removing, so the page does not do that a second time.
    fn handle_request(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.table.update(cx, |table, _| table.take_request()) else {
            return;
        };
        let config = self.session.read(cx).config(cx);
        let directory = config.translation.model_directory.clone();
        let site = config.translation.download_site;
        drop(config);

        match request {
            TranslationRequest::Download { model } => {
                self.start_model_download(model, site, false, cx);
            }
            TranslationRequest::Retry { model } => {
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
                downloads.update(cx, |downloads, cx| {
                    downloads.terminate(model, directory, cx);
                });
            }
            TranslationRequest::Delete { model } => {
                let (Some(directory), Some(downloads)) = (directory, self.downloads.clone()) else {
                    return;
                };
                downloads.update(cx, |downloads, cx| {
                    downloads.delete(model, directory, cx);
                });
            }
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
        let directory = self
            .session
            .read(cx)
            .config(cx)
            .translation
            .model_directory
            .clone();
        let Some(directory) = directory else {
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

    /// The directory the field shows, for the page's tests.
    #[cfg(test)]
    pub(crate) fn shown_directory(&self, cx: &gpui_kit::App) -> Option<SharedString> {
        self.directory
            .as_ref()
            .map(|input| input.read(cx).value(cx))
    }

    /// Chooses a model the way the open menu does, for the settings page's tests.
    #[cfg(test)]
    pub(crate) fn select_model_for_test(
        &mut self,
        model: TranslationModel,
        cx: &mut Context<Self>,
    ) {
        self.select_model(model, cx);
    }
}

impl Render for TranslationPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.handle_request(cx);

        let input = self.directory_input(window, cx);
        let config = self.session.read(cx).config(cx);
        let directory = config.translation.model_directory.clone();
        // What the menu offers is what the directory holds, minus what is being removed: a model
        // whose files are on their way out is not one the page can load, so it is not one the
        // user can pick.
        let downloaded = match (self.downloads.as_ref(), directory.as_deref()) {
            (Some(downloads), Some(directory)) => {
                let downloads = downloads.read(cx);
                downloads
                    .downloaded_models(directory)
                    .into_iter()
                    .filter(|model| !downloads.row_state(*model, directory).is_removing())
                    .collect::<Vec<_>>()
            }
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
                    page.update(cx, |page, cx| page.select_model(model, cx));
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
                                let change = SettingsChange::TranslationDownloadSite { site };
                                page.session.update(cx, |session, cx| {
                                    let _ = session.write(change, cx);
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
        session::SettingsSession,
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

    /// Opens a session over the settings a manager holds, as the settings page does.
    fn session_over(cx: &mut TestAppContext, config: Config) -> Entity<SettingsSession> {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
            cx.new(|_| SettingsSession::new())
        })
    }

    /// The settings as they are stored now.
    fn stored(cx: &TestAppContext) -> Config {
        cx.read(|cx| cx.global::<ConfigManager>().read().as_ref().clone())
    }

    /// Opens the page the way moving to it does, and lets its deferred repair run.
    fn open(page: &Entity<TranslationPage>, cx: &mut TestAppContext) {
        cx.update(|cx| page.update(cx, |page, cx| page.repair_on_open(cx)));
        cx.run_until_parked();
    }

    /// The table offers a download only when both the process-wide manager and a directory
    /// exist: without either there is nothing a download could start.
    #[gpui_kit::test]
    fn the_table_should_offer_no_download_without_a_manager_or_a_directory(
        cx: &mut TestAppContext,
    ) {
        let directory = TempDir::new().expect("the test directory builds");
        let downloads = downloads(cx);
        let session = session_over(cx, Config::default());

        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(session, None, cx)));

        assert!(
            !cx.read(|cx| page.read(cx).table.read(cx).downloads_enabled()),
            "a page without the process-wide manager cannot download"
        );

        let session = session_over(cx, Config::default());
        let page =
            cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads.clone()), cx)));

        assert!(
            !cx.read(|cx| page.read(cx).table.read(cx).downloads_enabled()),
            "a session with no directory cannot download"
        );

        let mut config = Config::default();
        config.translation.model_directory = Some(directory.path().to_path_buf());
        let session = session_over(cx, config);
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads), cx)));

        assert!(
            cx.read(|cx| page.read(cx).table.read(cx).downloads_enabled()),
            "the manager and a directory are what the download action needs"
        );
    }

    /// A directory the settings already hold is not replaced by the last download's one, even
    /// when a download is running in that other directory.
    #[gpui_kit::test]
    fn a_page_should_keep_the_directory_the_settings_already_have(cx: &mut TestAppContext) {
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
        let session = session_over(cx, config);
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads), cx)));

        open(&page, cx);

        assert_eq!(
            stored(cx).translation.model_directory,
            Some(chosen.path().to_path_buf()),
            "the page must not rewrite a directory the settings already hold"
        );
    }

    /// Settings opened without a directory adopt the one the last download completed in, and
    /// opening the page is what writes it.
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
        let session = session_over(cx, Config::default());
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads), cx)));

        assert_eq!(
            stored(cx).translation.model_directory,
            None,
            "building the page must not write the translation settings"
        );

        open(&page, cx);

        let config = stored(cx);
        assert_eq!(
            config.translation.model_directory,
            Some(directory.path().to_path_buf()),
            "a page with no directory should open on the one the download finished in"
        );
        assert_eq!(
            config.translation.selected_model, None,
            "opening the page must not select a model the user never chose"
        );
    }

    /// The repair runs once: a page opened again does not adopt what a later download left
    /// behind when the directory the settings hold moved on in between.
    #[gpui_kit::test]
    fn a_page_should_repair_its_settings_once(cx: &mut TestAppContext) {
        let first = TempDir::new().expect("the first directory builds");
        let second = TempDir::new().expect("the second directory builds");
        let downloads = downloads(cx);
        downloads.update(cx, |downloads, cx| {
            downloads.complete_download_for_test(
                TranslationModel::HyMt27BQ8,
                first.path().to_path_buf(),
                cx,
            );
        });
        let session = session_over(cx, Config::default());
        let page =
            cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads.clone()), cx)));

        open(&page, cx);
        assert_eq!(
            stored(cx).translation.model_directory,
            Some(first.path().to_path_buf())
        );

        // What a second repair would see: no stored directory, another download's directory.
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            manager.update(|config| config.translation.model_directory = None, cx);
        });
        downloads.update(cx, |downloads, _| {
            downloads.set_last_download_directory_for_test(second.path().to_path_buf());
        });
        open(&page, cx);

        assert_eq!(
            stored(cx).translation.model_directory,
            None,
            "the repair is made the first time the page is opened and never again"
        );
    }

    /// A selection the stored directory does not hold is cleared when the page is opened.
    #[gpui_kit::test]
    fn a_page_should_clear_a_selection_the_directory_does_not_hold(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let downloads = downloads(cx);
        let mut config = Config::default();
        config.translation.model_directory = Some(directory.path().to_path_buf());
        config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
        let session = session_over(cx, config);
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads), cx)));

        open(&page, cx);

        let config = stored(cx);
        assert_eq!(
            config.translation.selected_model, None,
            "a model the directory does not hold must not stay selected"
        );
        assert_eq!(
            config.translation.model_directory,
            Some(directory.path().to_path_buf()),
            "the directory itself is already stored and is not replaced"
        );
    }

    /// A selection whose files are on their way out is cleared even though the file is still
    /// there when the page opens.
    #[gpui_kit::test]
    fn a_page_should_clear_a_selection_that_is_being_removed(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let model = TranslationModel::HyMt27BQ8;
        let downloads = downloads(cx);
        downloads.update(cx, |downloads, cx| {
            downloads.force_downloaded_for_test(model, directory.path().to_path_buf(), cx);
            downloads.set_state_for_test(
                model,
                directory.path().to_path_buf(),
                DownloadState::Deleting,
                cx,
            );
        });
        let mut config = Config::default();
        config.translation.model_directory = Some(directory.path().to_path_buf());
        config.translation.selected_model = Some(model);
        let session = session_over(cx, config);
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads), cx)));

        open(&page, cx);

        assert_eq!(
            stored(cx).translation.selected_model,
            None,
            "a model being removed must not stay selected"
        );
    }

    /// A model can only be selected while the directory holds it and its row is not being
    /// removed; a choice that stopped being available clears the selection instead of writing
    /// a model the page cannot load.
    #[gpui_kit::test]
    fn a_model_should_only_be_selected_while_the_directory_can_load_it(cx: &mut TestAppContext) {
        let directory = TempDir::new().expect("the test directory builds");
        let downloaded = TranslationModel::HyMt218BQ4Km;
        let missing = TranslationModel::HyMt27BQ8;
        let removing = TranslationModel::HyMt218BQ6K;
        let downloads = downloads(cx);
        downloads.update(cx, |downloads, cx| {
            downloads.force_downloaded_for_test(downloaded, directory.path().to_path_buf(), cx);
            downloads.force_downloaded_for_test(removing, directory.path().to_path_buf(), cx);
            downloads.set_state_for_test(
                removing,
                directory.path().to_path_buf(),
                DownloadState::Deleting,
                cx,
            );
        });
        let mut config = Config::default();
        config.translation.model_directory = Some(directory.path().to_path_buf());
        let session = session_over(cx, config);
        let page = cx.update(|cx| cx.new(|cx| TranslationPage::new(session, Some(downloads), cx)));

        cx.update(|cx| page.update(cx, |page, cx| page.select_model(downloaded, cx)));
        assert_eq!(
            stored(cx).translation.selected_model,
            Some(downloaded),
            "a downloaded model the directory holds is selected"
        );

        cx.update(|cx| page.update(cx, |page, cx| page.select_model(missing, cx)));
        assert_eq!(
            stored(cx).translation.selected_model,
            None,
            "a model the directory does not hold clears the selection"
        );

        cx.update(|cx| page.update(cx, |page, cx| page.select_model(removing, cx)));
        assert_eq!(
            stored(cx).translation.selected_model,
            None,
            "a model whose files are being removed clears the selection"
        );
    }
}

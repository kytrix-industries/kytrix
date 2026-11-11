//! The translation page: where models are stored, which one is used, and the models that can be
//! downloaded.
//!
//! The page is a view of its own rather than a page of fields: its model list is a table of its
//! own, its directory is a label that only the system folder picker changes, and its download
//! state belongs to the process-wide [`crate::translation_downloads::TranslationDownloads`]
//! rather than to the settings page. The chosen directory and model are written into the settings
//! as they are chosen, and the controls read them back from there, so what the page draws is what
//! is stored.
//!
//! One repair is made the first time the page is really drawn, and only then — see
//! [`TranslationPage::repair_on_open`]: a selection that the stored directory does not hold is
//! cleared. It is written once, so a page opened without being entered writes nothing.
//!
//! Downloading starts from the site the settings hold: the site is a select in the card of model
//! settings, and both a first download and a retry of a failed one use the chosen site.

use std::path::PathBuf;

use app_config::translation::{DownloadSite, TranslationModel};
use base_ui::{
    button::Button, interactive::InteractiveVariantExt, style::theme::ThemeExt, tooltip::Tooltip,
};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement, PathPromptOptions, Render, SharedString, StatefulInteractiveElement as _,
    Styled, Subscription, Window, div,
};
use i18n::t;

use super::table::{TranslationRequest, TranslationTable};
use crate::{
    layout,
    search::{PageSearch, words},
    session::{SettingsChange, SettingsSession},
    translation_downloads::TranslationDownloads,
};

/// What one site is called in the interface.
fn site_label(site: DownloadSite, cx: &App) -> SharedString {
    match site {
        DownloadSite::HuggingFace => t!(cx, "settings_ui.ai.download_site_huggingface"),
        DownloadSite::ModelScope => t!(cx, "settings_ui.ai.download_site_modelscope"),
    }
}

/// The stored id of one site, which is what its choice writes.
const fn site_id(site: DownloadSite) -> &'static str {
    match site {
        DownloadSite::HuggingFace => "hugging_face",
        DownloadSite::ModelScope => "model_scope",
    }
}

/// The site a choice names.
fn site_from_id(id: &str) -> Option<DownloadSite> {
    DownloadSite::ALL
        .into_iter()
        .find(|site| site_id(*site) == id)
}

/// The text the translation page is searched by.
///
/// The page has no setting rows of its own to filter: its directory, its site and its model are
/// delivered by the page as a whole, and the model table is a view over the directory. What the
/// search reaches it by is therefore the page's own words, including the names of the settings it
/// holds.
pub(crate) fn search(cx: &App) -> PageSearch {
    let title = t!(cx, "settings_ui.ai.translation.title");
    let directory = t!(cx, "settings_ui.ai.model_directory");
    let site = t!(cx, "settings_ui.ai.download_site");
    let model = t!(cx, "settings_ui.ai.model_in_use");
    let models = t!(cx, "settings_ui.ai.models");

    PageSearch {
        label: words(&[
            title.as_ref(),
            directory.as_ref(),
            site.as_ref(),
            model.as_ref(),
            models.as_ref(),
            "translation",
            "translate",
            "翻译",
            "model",
            "models",
            "模型",
            "当前支持的模型",
            "模型存放目录",
            "使用的模型",
            "model directory",
            "model in use",
            "browse",
            "浏览",
            "huggingface",
            "modelscope",
            "魔搭",
            "download site",
            "model download site",
            "下载站点",
            "模型下载站点",
            "站点",
            "download",
            "下载",
            "hy-mt2",
            "hy mt2",
        ]),
        fields: Vec::new(),
    }
}

/// The translation page.
pub(crate) struct TranslationPage {
    session: Entity<SettingsSession>,
    /// The process-wide downloads, when the composition root installed them.
    downloads: Option<Entity<TranslationDownloads>>,
    /// The supported models and what each of them is doing.
    table: Entity<TranslationTable>,
    /// Whether the repair of [`Self::repair_on_open`] has been scheduled, so it happens once.
    repair_started: bool,
    /// Held because a write of the settings can land here rather than through this page — the
    /// repair the page schedules is written through the session — and the directory label, the
    /// site radios and the model select all draw what the settings hold.
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
            downloads,
            table,
            repair_started: false,
            _session: session_changes,
            _observe: observe,
        }
    }

    /// Schedules the repair the page makes, once, when it is first opened.
    ///
    /// A settings page that is opened but never shows this page must not write the translation
    /// settings, so nothing is written when the page is built: the repair is scheduled here,
    /// when the page is moved to, and runs at the end of the effect cycle that moved there.
    pub(crate) fn repair_on_open(&mut self, cx: &mut Context<Self>) {
        if self.repair_started {
            return;
        }
        self.repair_started = true;

        let session = self.session.clone();
        let downloads = self.downloads.clone();
        cx.defer(move |cx| Self::repair(session, downloads, cx));
    }

    /// Clears a selection the stored directory does not hold.
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
        let directory = config.translation.model_directory.clone();
        let selected = config.translation.selected_model;
        drop(config);

        let Some(selected) = selected else {
            return;
        };
        let available = directory
            .as_deref()
            .is_some_and(|directory| downloads.read(cx).is_available(selected, directory));
        if available {
            return;
        }

        session.update(cx, |session, cx| {
            let change = SettingsChange::TranslationModelSelected { model: None };
            let _ = session.write(change, cx);
        });
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
            let _ = page.update_in(cx, |page, _window, cx| page.choose(directory, cx));
        })
        .detach();
    }

    /// Writes `directory` as the model directory and shows it in the label.
    ///
    /// The selection follows the directory in the same write: a selection the new directory
    /// cannot load would name a file that is not there, so the first model the directory holds —
    /// in the order their names sort — is selected instead, and the selection is cleared when the
    /// directory holds none. A selection the new directory does hold is left as it is. The label
    /// shows the directory only once it is stored — a write that failed leaves the old one on
    /// screen, and the settings page reports why.
    fn choose(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        let (unchanged, selected) = {
            let config = self.session.read(cx).config(cx);
            let unchanged =
                config.translation.model_directory.as_deref() == Some(directory.as_path());
            let selected = config.translation.selected_model;

            (unchanged, selected)
        };
        if unchanged {
            return;
        }

        let next = {
            let mut available = self
                .downloads
                .as_ref()
                .map(|downloads| downloads.read(cx).available_models(&directory))
                .unwrap_or_default();
            available.sort_by_key(|model| model.display_name());

            match selected {
                Some(model) if available.contains(&model) => Some(model),
                _ => available.first().copied(),
            }
        };

        let mut changes = Vec::with_capacity(2);
        changes.push(SettingsChange::TranslationModelDirectory { directory });
        if next != selected {
            changes.push(SettingsChange::TranslationModelSelected { model: next });
        }

        let stored = self
            .session
            .update(cx, |session, cx| session.write_all(changes, cx));
        if stored.is_err() {
            return;
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

    /// The directory the page shows, for the page's tests.
    #[cfg(test)]
    pub(crate) fn shown_directory(&self, cx: &gpui_kit::App) -> Option<SharedString> {
        let config = self.session.read(cx).config(cx);

        config
            .translation
            .model_directory
            .as_ref()
            .map(|path| SharedString::from(path.to_string_lossy().into_owned()))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.handle_request(cx);

        let config = self.session.read(cx).config(cx);
        let directory = config.translation.model_directory.clone();
        // What the menu offers is what the directory holds, minus what is being removed: a model
        // whose files are on their way out is not one the page can load, so it is not one the
        // user can pick.
        let downloaded = self
            .downloads
            .as_ref()
            .zip(directory.as_deref())
            .map(|(downloads, directory)| downloads.read(cx).available_models(directory))
            .unwrap_or_default();
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

        let model_page = cx.entity().downgrade();
        let model_label = t!(cx, "settings_ui.ai.model_in_use");
        let model_choices = downloaded.iter().map(|model| {
            (
                SharedString::from(model.display_name()),
                SharedString::from(model.display_name()),
            )
        });
        let model_select = layout::choice_select(
            ElementId::Name(SharedString::new_static("settings-ai-model-select")),
            selected.map(|model| SharedString::from(model.display_name())),
            model_choices,
            model_label.clone(),
            Some(placeholder.into_any_element()),
            disabled,
            move |item_id, _, cx| {
                let Some(model) = item_id
                    .as_ref()
                    .and_then(|id| TranslationModel::from_name(id.as_str()))
                else {
                    return;
                };
                if let Some(page) = model_page.upgrade() {
                    page.update(cx, |page, cx| page.select_model(model, cx));
                }
            },
        );

        let download_site = config.translation.download_site;
        let site_page = cx.entity().downgrade();
        let site_label_text = t!(cx, "settings_ui.ai.download_site");
        let site_choices = DownloadSite::ALL.into_iter().map(|site| {
            (
                SharedString::new_static(site_id(site)),
                site_label(site, cx),
            )
        });
        let site_select = layout::choice_select(
            ElementId::Name(SharedString::new_static("settings-ai-site-select")),
            Some(SharedString::new_static(site_id(download_site))),
            site_choices,
            site_label_text,
            None,
            false,
            move |item_id, _, cx| {
                let Some(site) = item_id.as_ref().and_then(|id| site_from_id(id.as_str())) else {
                    return;
                };
                let Some(page) = site_page.upgrade() else {
                    return;
                };
                page.update(cx, |page, cx| {
                    let change = SettingsChange::TranslationDownloadSite { site };
                    page.session.update(cx, |session, cx| {
                        let _ = session.write(change, cx);
                    });
                    cx.notify();
                });
            },
        );

        let directory_text = directory
            .as_ref()
            .map_or_else(SharedString::default, |path| {
                SharedString::from(path.to_string_lossy().into_owned())
            });
        let directory_label = div()
            .debug_selector(|| "SETTINGS_AI_DIRECTORY_LABEL".into())
            .min_w_0()
            .truncate()
            .text_sm()
            .text_color(cx.theme().roles.fg.muted.hsla)
            .child(directory_text.clone());
        let mut directory_hint = Tooltip::new(
            ElementId::Name(SharedString::new_static("settings-ai-directory-tooltip")),
            directory_label,
        )
        .min_w_0();
        if !directory_text.is_empty() {
            directory_hint = directory_hint.content(directory_text);
        }

        let directory_control = div()
            .w_full()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pr_2()
                    .flex()
                    .items_center()
                    .child(directory_hint),
            )
            .child(
                Button::new("settings-ai-browse")
                    .tonal()
                    .flex_none()
                    .debug_selector(|| "SETTINGS_AI_BROWSE".into())
                    .aria_label(t!(cx, "settings_ui.ai.browse"))
                    .child(t!(cx, "settings_ui.ai.browse"))
                    .on_click(cx.listener(|page, _, window, cx| {
                        page.browse(window, cx);
                    })),
            )
            .into_any_element();

        let directory_row = div()
            .debug_selector(|| "SETTINGS_AI_DIRECTORY".into())
            .child(layout::row(
                "ai.directory",
                t!(cx, "settings_ui.ai.model_directory"),
                t!(cx, "settings_ui.ai.model_directory_description"),
                directory_control,
                layout::CONTROL_WIDTH,
                cx,
            ))
            .into_any_element();
        let site_row = div()
            .debug_selector(|| "SETTINGS_AI_DOWNLOAD_SITE".into())
            .child(layout::row(
                "ai.download-site",
                t!(cx, "settings_ui.ai.download_site"),
                t!(cx, "settings_ui.ai.download_site_description"),
                div()
                    .w_full()
                    .min_w_0()
                    .debug_selector(|| "SETTINGS_AI_SITE_SELECT".into())
                    .child(site_select)
                    .into_any_element(),
                layout::CONTROL_WIDTH,
                cx,
            ))
            .into_any_element();
        let model_row = div()
            .debug_selector(|| "SETTINGS_AI_MODEL_SELECT_SECTION".into())
            .child(layout::row(
                "ai.model",
                model_label,
                t!(cx, "settings_ui.ai.model_in_use_description"),
                div()
                    .w_full()
                    .min_w_0()
                    .debug_selector(|| "SETTINGS_AI_MODEL_SELECT".into())
                    .child(model_select)
                    .into_any_element(),
                layout::CONTROL_WIDTH,
                cx,
            ))
            .into_any_element();

        div()
            .debug_selector(|| "SETTINGS_AI_PAGE".into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_4()
            .py_1()
            .child(layout::section(
                None,
                "ai.translation",
                vec![directory_row, site_row, model_row],
                cx,
            ))
            .child(
                div()
                    .debug_selector(|| "SETTINGS_AI_MODELS".into())
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(layout::section_title(t!(cx, "settings_ui.ai.models"), cx))
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

        let mut config = Config::default();
        config.translation.model_directory = None;
        let session = session_over(cx, config);
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

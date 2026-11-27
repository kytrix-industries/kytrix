use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use app_config::translation::{DownloadSite, TranslationModel};
use i18n::{t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, Event},
    math::px,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{style::PaintSpec, style_builder::StyleBuilder},
    window::WindowId,
};
use ui_kit::{
    kit::{
        input::{self, InputNode, InputSpec, InputState},
        select::{Select, SelectItem, SelectOptions},
    },
    theme::{components::ControlSize, role::Role},
};

use super::table::TranslationTable;
use crate::{
    host::SettingsHost,
    layout,
    pages::page::{PageBuild, PageView},
    search::{PageSearch, words},
    session::SettingsChange,
    translation_downloads::TranslationDownloads,
};

const PAGE: &str = "ai.translation";

pub(crate) struct TranslationPage {
    host: SettingsHost,
    key: SmolStr,
    window: Option<WindowId>,
    downloads: Option<Rc<TranslationDownloads>>,
    directory: Option<InputNode>,
    directory_state: Option<Signal<InputState>>,
    directory_rejected: Rc<Cell<bool>>,
    site: Option<Select>,
    model: Option<Select>,
    model_parent: Option<NodeId>,
    model_items: Vec<SmolStr>,
    model_has_directory: Option<bool>,
    table: Option<TranslationTable>,
    commit: Option<Action>,
}

impl TranslationPage {
    pub(crate) fn new(host: SettingsHost, key: &str) -> Self {
        Self {
            host,
            key: SmolStr::new(key),
            window: None,
            downloads: None,
            directory: None,
            directory_state: None,
            directory_rejected: Rc::new(Cell::new(false)),
            site: None,
            model: None,
            model_parent: None,
            model_items: Vec::new(),
            model_has_directory: None,
            table: None,
            commit: None,
        }
    }

    fn repair(&self, app: &mut App, window: WindowId) {
        let Some(downloads) = self.downloads.as_deref() else {
            return;
        };
        let (directory, selected) = self.host.config(app, |config| {
            (
                config.translation.model_directory.clone(),
                config.translation.selected_model,
            )
        });
        let Some(selected) = selected else {
            return;
        };
        let available = directory
            .as_deref()
            .is_some_and(|directory| downloads.is_available(selected, directory));
        if available {
            return;
        }

        let _ = self.host.write(
            app,
            window,
            SettingsChange::TranslationModelSelected { model: None },
        );
    }

    fn create_model_select(
        &self,
        app: &mut App,
        window: WindowId,
        directory: Option<&std::path::Path>,
        available: &[TranslationModel],
        selected: Option<TranslationModel>,
    ) -> Result<Select, EngineError> {
        let translate = self.host.translate().clone();
        let label = t_smol!(translate, "settings_ui.ai.model_in_use");
        let placeholder = if directory.is_some() {
            t_smol!(translate, "settings_ui.ai.no_downloaded_models")
        } else {
            t_smol!(translate, "settings_ui.ai.no_model_directory")
        };
        let items = available
            .iter()
            .map(|model| {
                SelectItem::new(
                    SmolStr::new(model.display_name()),
                    SmolStr::new(model.display_name()),
                )
            })
            .collect::<Vec<_>>();
        let chosen = selected
            .filter(|model| available.contains(model))
            .map(|model| SmolStr::new(model.display_name()));
        let key = format_smolstr!("{}.{PAGE}.model", self.key);
        let select = Select::new(
            app,
            self.host.theme_handle(),
            key,
            SelectOptions::default()
                .width(layout::CONTROL_WIDTH)
                .label(label)
                .placeholder(placeholder),
        );
        select.set_items(app, items)?;
        match chosen {
            Some(chosen) => select.set_chosen(app, [chosen.as_str()], false),
            None => select.set_chosen(app, std::iter::empty::<&str>(), false),
        }
        select.set_disabled(app, directory.is_none() || available.is_empty());

        let host = self.host.clone();
        let downloads = self.downloads.clone();
        Ok(select.on_change(move |app, chosen| {
            let Some(model) = chosen
                .first()
                .and_then(|id| TranslationModel::from_name(id.as_str()))
            else {
                return;
            };
            select_model(app, &host, window, downloads.as_deref(), model);
        }))
    }

    fn rebuild_model(
        &mut self,
        app: &mut App,
        window: WindowId,
        directory: Option<&std::path::Path>,
        available: &[TranslationModel],
        selected: Option<TranslationModel>,
    ) -> Result<(), EngineError> {
        let Some(parent) = self.model_parent else {
            return Ok(());
        };
        if let Some(select) = self.model.take() {
            select.release(app);
        }
        for child in child_nodes(app, window, parent) {
            if let Err(error) = app.remove_node(window, child) {
                log::warn!("removing the rebuilt model select failed: {error}");
            }
        }
        let select = self.create_model_select(app, window, directory, available, selected)?;
        let _ = select.mount(app, window, parent)?;
        self.model = Some(select);
        self.model_has_directory = Some(directory.is_some());
        self.model_items = available
            .iter()
            .map(|model| SmolStr::new(model.display_name()))
            .collect();

        Ok(())
    }
}

impl PageView for TranslationPage {
    fn page_id(&self) -> &'static str {
        PAGE
    }

    fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        cx: &PageBuild<'_>,
    ) -> Result<(), EngineError> {
        self.window = Some(window);
        self.downloads = self.host.downloads().cloned();
        self.repair(app, window);

        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();
        let (directory, site, selected) = self.host.config(app, |config| {
            (
                config.translation.model_directory.clone(),
                config.translation.download_site,
                config.translation.selected_model,
            )
        });
        let available = match (self.downloads.as_deref(), directory.as_deref()) {
            (Some(downloads), Some(directory)) => downloads.available_models(directory),
            _ => Vec::new(),
        };

        let root = layout::themed_box(
            app,
            window,
            parent,
            SmolStr::new_static("SETTINGS_AI_PAGE"),
            StyleBuilder::new()
                .flex_col()
                .size_full()
                .min_width_0()
                .min_height_0()
                .gap(px(16.0))
                .padding_y(px(4.0))
                .build(),
            PaintSpec::default(),
            &theme,
            None,
            None,
        )?;

        let directory_state = app.signal(InputState::new(
            directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ));
        let placeholder = app.signal(t_smol!(translate, "settings_ui.ai.model_directory"));
        self.directory_state = Some(directory_state);
        let commit = Action::Custom(format_smolstr!("{}.{PAGE}.directory.commit", self.key));
        let spec = Arc::new(InputSpec {
            name: format_smolstr!("{}.{PAGE}.directory", self.key),
            label: None,
            placeholder: t_smol!(translate, "settings_ui.ai.model_directory"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Medium,
            submit: Some(commit.clone()),
        });
        let directory_slot = Rc::new(RefCell::new(None::<InputNode>));
        {
            let rejected = Rc::clone(&self.directory_rejected);
            let host = self.host.clone();
            let downloads = self.downloads.clone();
            let slot = Rc::clone(&directory_slot);
            app.on_action(commit.clone(), move |ctx| {
                let Some(input) = slot.borrow().clone() else {
                    return;
                };
                let text = input.value(ctx.app_mut());
                if text.is_empty() {
                    return;
                }
                let window = ctx.window();
                let accepted = choose_directory(
                    ctx.app_mut(),
                    &host,
                    window,
                    downloads.as_deref(),
                    PathBuf::from(text.as_str()),
                );
                rejected.set(!accepted);
            });
        }
        self.commit = Some(commit);

        let mut rows = Vec::with_capacity(3);
        let directory_theme = theme.clone();
        let directory_row_theme = theme.clone();
        let directory_translate = translate.clone();
        let directory_sink = Rc::clone(&directory_slot);
        let directory_spec = Arc::clone(&spec);
        rows.push(layout::row(
            &directory_row_theme,
            "ai.directory",
            t_smol!(directory_translate, "settings_ui.ai.model_directory"),
            t_smol!(
                directory_translate,
                "settings_ui.ai.model_directory_description"
            ),
            move |app, window, parent| {
                let wrapper = layout::build_node(app, window, parent, |tree, parent| {
                    NodeBuilder::boxed()
                        .id(SmolStr::new_static("SETTINGS_AI_DIRECTORY_INPUT"))
                        .style(StyleBuilder::new().width_full().min_width_0().build())
                        .build(tree, parent)
                })?;
                let input = {
                    let tree = app
                        .window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut();
                    input::build(
                        tree,
                        wrapper,
                        Arc::clone(&directory_spec),
                        &directory_theme,
                        &directory_state,
                        &placeholder,
                    )
                };
                input.install(app);
                input.set_submit_on_blur(true);
                let _ = input.sync(app);
                *directory_sink.borrow_mut() = Some(input);
                Ok(())
            },
        ));

        let site_row_label = t_smol!(translate, "settings_ui.ai.download_site");
        let site_choices = DownloadSite::ALL
            .into_iter()
            .map(|choice| {
                (
                    SmolStr::new_static(site_id(choice)),
                    site_label(choice, &translate),
                )
            })
            .collect::<Vec<_>>();
        let site_host = self.host.clone();
        let site_select = layout::choice_select(
            app,
            &self.host,
            cx.unique_key("site"),
            site_id(site),
            site_choices,
            site_row_label.clone(),
            move |app, value| {
                let Some(chosen) = site_of(value) else {
                    return;
                };
                let _ = site_host.write(
                    app,
                    window,
                    SettingsChange::TranslationDownloadSite { site: chosen },
                );
            },
        );
        let site_for_row = site_select.clone();
        let site_theme = theme.clone();
        let site_translate = translate.clone();
        rows.push(layout::row(
            &site_theme,
            "ai.download-site",
            site_row_label,
            t_smol!(site_translate, "settings_ui.ai.download_site_description"),
            move |app, window, parent| {
                let wrapper = layout::build_node(app, window, parent, |tree, parent| {
                    NodeBuilder::boxed()
                        .id(SmolStr::new_static("SETTINGS_AI_SITE_SELECT"))
                        .style(StyleBuilder::new().width_full().min_width_0().build())
                        .build(tree, parent)
                })?;
                let _ = site_for_row.mount(app, window, wrapper)?;
                Ok(())
            },
        ));
        self.site = Some(site_select);

        let model_parent = Rc::new(Cell::new(None::<NodeId>));
        let model_parent_for_row = Rc::clone(&model_parent);
        let model_select =
            self.create_model_select(app, window, directory.as_deref(), &available, selected)?;
        self.model_has_directory = Some(directory.is_some());
        self.model_items = available
            .iter()
            .map(|model| SmolStr::new(model.display_name()))
            .collect();
        let model_for_row = model_select.clone();
        let model_theme = theme.clone();
        let model_translate = translate.clone();
        rows.push(layout::row(
            &model_theme,
            "ai.model",
            t_smol!(model_translate, "settings_ui.ai.model_in_use"),
            t_smol!(model_translate, "settings_ui.ai.model_in_use_description"),
            move |app, window, parent| {
                let wrapper = layout::build_node(app, window, parent, |tree, parent| {
                    NodeBuilder::boxed()
                        .id(SmolStr::new_static("SETTINGS_AI_MODEL_SELECT"))
                        .style(StyleBuilder::new().width_full().min_width_0().build())
                        .build(tree, parent)
                })?;
                let _ = model_for_row.mount(app, window, wrapper)?;
                model_parent_for_row.set(Some(wrapper));
                Ok(())
            },
        ));
        self.model = Some(model_select);
        self.model_parent = model_parent.get();

        layout::section(app, window, root, &theme, "ai.translation", None, rows)?;
        self.directory = directory_slot.borrow().clone();

        if cx.filter.admits_field(PAGE, "models") {
            let models = layout::build_node(app, window, root, |tree, parent| {
                NodeBuilder::boxed()
                    .id(SmolStr::new_static("SETTINGS_AI_MODELS"))
                    .style(
                        StyleBuilder::new()
                            .flex_col()
                            .flex_1()
                            .width_full()
                            .min_width_0()
                            .min_height_0()
                            .gap(px(6.0))
                            .build(),
                    )
                    .build(tree, parent)
            })?;
            layout::themed_text(
                app,
                window,
                models,
                Some(SmolStr::new_static("SETTINGS_AI_MODELS_TITLE")),
                t_smol!(translate, "settings_ui.ai.models"),
                13.0,
                600,
                Role::FgDefault,
                &theme,
            )?;
            let mut table = TranslationTable::new(cx.unique_key("model"), self.downloads.clone());
            table.build(
                app,
                window,
                models,
                &theme,
                &translate,
                directory.as_deref(),
                site,
            )?;
            self.table = Some(table);
        }

        Ok(())
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        let Some(window) = self.window else {
            return Ok(());
        };
        if app.window(window).is_none() {
            return Ok(());
        }

        let mut changed = false;
        if let Some(downloads) = &self.downloads {
            changed |= downloads.sync(app);
        }

        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();

        if let Some(input) = &self.directory {
            changed |= input.sync(app);
            let stored = self
                .host
                .config(app, |config| config.translation.model_directory.clone())
                .as_ref()
                .map(|path| SmolStr::new(path.to_string_lossy()))
                .unwrap_or_default();
            let rejected = self.directory_rejected.replace(false);
            let state = match self.directory_state {
                Some(state) => {
                    let state = app.read(&state);
                    (SmolStr::new(state.text()), state.is_focused())
                }
                None => (SmolStr::new(""), false),
            };
            if rejected || (state.0 != stored && !state.1) {
                changed |= input.update_value(app, stored.as_str());
            }
        }

        let (directory, site, selected) = self.host.config(app, |config| {
            (
                config.translation.model_directory.clone(),
                config.translation.download_site,
                config.translation.selected_model,
            )
        });

        if let Some(select) = &self.site {
            let chosen = select.chosen(app);
            if chosen.as_slice() != [SmolStr::new_static(site_id(site))] {
                select.set_chosen(app, [site_id(site)], false);
                changed = true;
            }
        }

        let available = match (self.downloads.as_deref(), directory.as_deref()) {
            (Some(downloads), Some(directory)) => downloads.available_models(directory),
            _ => Vec::new(),
        };
        let items = available
            .iter()
            .map(|model| SmolStr::new(model.display_name()))
            .collect::<Vec<_>>();
        if self.model_has_directory != Some(directory.is_some()) {
            self.rebuild_model(app, window, directory.as_deref(), &available, selected)?;
            changed = true;
        } else if let Some(select) = &self.model {
            if items != self.model_items {
                select.set_items(
                    app,
                    items
                        .iter()
                        .map(|id| SelectItem::new(id.clone(), id.clone())),
                )?;
                self.model_items = items;
                changed = true;
            }
            let wanted = selected
                .filter(|model| available.contains(model))
                .map(|model| SmolStr::new(model.display_name()));
            let chosen = select.chosen(app);
            let matches = match (&wanted, chosen.as_slice()) {
                (Some(wanted), [chosen]) => wanted == chosen,
                (None, []) => true,
                _ => false,
            };
            if !matches {
                match &wanted {
                    Some(wanted) => select.set_chosen(app, [wanted.as_str()], false),
                    None => select.set_chosen(app, std::iter::empty::<&str>(), false),
                }
                changed = true;
            }
            changed |= select.set_disabled(app, directory.is_none() || available.is_empty());
        }
        if let Some(select) = &self.site {
            changed |= select.sync(app)?;
        }
        if let Some(select) = &self.model {
            changed |= select.sync(app)?;
        }

        if let Some(table) = &mut self.table {
            changed |= table.sync(app, window, &theme, &translate, directory.as_deref(), site)?;
        }

        if changed {
            let _ = app.request_repaint(window);
        }

        Ok(())
    }

    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        let mut consumed = false;
        if let Some(select) = &self.site {
            consumed |= select.dispatch(app, event);
        }
        if let Some(select) = &self.model {
            consumed |= select.dispatch(app, event);
        }
        consumed
    }

    fn release(&mut self, app: &mut App) {
        if let Some(input) = self.directory.take() {
            input.release(app);
        }
        if let Some(select) = self.site.take() {
            select.release(app);
        }
        if let Some(select) = self.model.take() {
            select.release(app);
        }
        if let Some(mut table) = self.table.take() {
            table.release(app);
        }
        if let Some(action) = self.commit.take() {
            app.off_action(&action);
        }
        self.model_items.clear();
        self.model_has_directory = None;
        self.model_parent = None;
    }

    fn scrolls(&self) -> bool {
        false
    }
}

fn site_label(site: DownloadSite, translate: &Translate) -> SmolStr {
    match site {
        DownloadSite::HuggingFace => t_smol!(translate, "settings_ui.ai.download_site_huggingface"),
        DownloadSite::ModelScope => t_smol!(translate, "settings_ui.ai.download_site_modelscope"),
    }
}

fn site_id(site: DownloadSite) -> &'static str {
    match site {
        DownloadSite::HuggingFace => "hugging_face",
        DownloadSite::ModelScope => "model_scope",
    }
}

fn site_of(id: &str) -> Option<DownloadSite> {
    DownloadSite::ALL
        .into_iter()
        .find(|site| site_id(*site) == id)
}

fn select_model(
    app: &mut App,
    host: &SettingsHost,
    window: WindowId,
    downloads: Option<&TranslationDownloads>,
    model: TranslationModel,
) {
    let available = host.config(app, |config| {
        config
            .translation
            .model_directory
            .as_deref()
            .is_some_and(|directory| {
                downloads.is_some_and(|downloads| {
                    !downloads.row_state(model, directory).is_removing()
                        && downloads.is_downloaded(model, directory)
                })
            })
    });

    let selected = available.then_some(model);
    let _ = host.write(
        app,
        window,
        SettingsChange::TranslationModelSelected { model: selected },
    );
}

fn choose_directory(
    app: &mut App,
    host: &SettingsHost,
    window: WindowId,
    downloads: Option<&TranslationDownloads>,
    directory: PathBuf,
) -> bool {
    let (unchanged, selected) = host.config(app, |config| {
        (
            config.translation.model_directory.as_deref() == Some(directory.as_path()),
            config.translation.selected_model,
        )
    });
    if unchanged {
        return true;
    }

    let next = {
        let mut available = downloads
            .map(|downloads| downloads.available_models(&directory))
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
    host.write_all(app, window, changes).is_ok()
}

fn child_nodes(app: &App, window: WindowId, parent: NodeId) -> Vec<NodeId> {
    app.window(window)
        .and_then(|target| target.tree().get(parent).ok())
        .map(|entry| entry.children().to_vec())
        .unwrap_or_default()
}

pub(crate) fn search(translate: &Translate) -> PageSearch {
    let title = t_smol!(translate, "settings_ui.ai.translation.title");
    let directory = t_smol!(translate, "settings_ui.ai.model_directory");
    let site = t_smol!(translate, "settings_ui.ai.download_site");
    let model = t_smol!(translate, "settings_ui.ai.model_in_use");
    let models = t_smol!(translate, "settings_ui.ai.models");
    let browse = t_smol!(translate, "settings_ui.ai.browse");

    PageSearch {
        label: words(&[
            title.as_ref(),
            directory.as_ref(),
            site.as_ref(),
            model.as_ref(),
            models.as_ref(),
            browse.as_ref(),
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

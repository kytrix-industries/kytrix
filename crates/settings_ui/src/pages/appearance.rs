//! The appearance page: the application's theme and the language it is drawn in.

use app_config::settings::{Appearance, Language};
use base_ui::kit::select::Select;
use i18n::translate::Translate;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, event::Event, node::id::NodeId, window::WindowId};

use crate::{
    host::SettingsHost,
    layout,
    pages::page::{PageBuild, PageView},
    search::{FieldSearch, PageSearch, words},
    session::SettingsChange,
};

/// The id of this page's settings.
const PAGE: &str = "appearance";

/// The theme choices, in the order the control offers them.
const APPEARANCE_OPTIONS: [(&str, &str); 3] = [
    ("Light", "settings_ui.appearance.light"),
    ("Dark", "settings_ui.appearance.dark"),
    ("System", "settings_ui.appearance.system"),
];

/// The language choices, in the order the control offers them.
const LANGUAGE_OPTIONS: [(&str, &str); 2] = [
    ("zh-CN", "settings_ui.appearance.language_zh_cn"),
    ("en", "settings_ui.appearance.language_en"),
];

/// The appearance page.
pub(crate) struct AppearancePage {
    host: SettingsHost,
    key: SmolStr,
    window: Option<WindowId>,
    appearance: Option<Select>,
    language: Option<Select>,
    /// The setting the two controls were last written from, so a frame that moved nothing writes
    /// nothing.
    applied: Option<(Appearance, Language)>,
}

impl AppearancePage {
    /// The page over the injected collaborators, keyed by the settings page instance it belongs to.
    pub(crate) fn new(host: SettingsHost, key: &str) -> Self {
        Self {
            host,
            key: SmolStr::new(key),
            window: None,
            appearance: None,
            language: None,
            applied: None,
        }
    }
}

impl PageView for AppearancePage {
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
        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();
        let config = self.host.config(app, |config| config.clone());
        let mut rows = Vec::with_capacity(2);

        if cx.filter.admits_field(PAGE, "appearance") {
            let label = i18n::t_smol!(translate, "settings_ui.appearance.mode");
            let description = i18n::t_smol!(translate, "settings_ui.appearance.mode_description");
            let choices = APPEARANCE_OPTIONS
                .into_iter()
                .map(|(id, label)| (SmolStr::new_static(id), i18n::t_smol!(translate, label)))
                .collect::<Vec<_>>();
            let host = self.host.clone();
            let select = layout::choice_select(
                app,
                &self.host,
                format_smolstr!("{}.appearance.appearance", self.key),
                appearance_id(config.appearance),
                choices,
                label.clone(),
                move |app, value| {
                    let appearance = appearance_of(value);
                    let _ = host.write(
                        app,
                        window,
                        SettingsChange::edit(move |config| config.appearance = appearance),
                    );
                },
            );
            let control = select.clone();
            rows.push(layout::row(
                &theme,
                "appearance",
                label,
                description,
                move |app, window, parent| {
                    control.mount(app, window, parent)?;
                    Ok(())
                },
            ));
            self.appearance = Some(select);
        }

        if cx.filter.admits_field(PAGE, "language") {
            let label = i18n::t_smol!(translate, "settings_ui.appearance.language");
            let description =
                i18n::t_smol!(translate, "settings_ui.appearance.language_description");
            let choices = LANGUAGE_OPTIONS
                .into_iter()
                .map(|(id, label)| (SmolStr::new_static(id), i18n::t_smol!(translate, label)))
                .collect::<Vec<_>>();
            let host = self.host.clone();
            let select = layout::choice_select(
                app,
                &self.host,
                format_smolstr!("{}.appearance.language", self.key),
                config.language.locale(),
                choices,
                label.clone(),
                move |app, value| {
                    let language = language_of(value);
                    let _ = host.write(
                        app,
                        window,
                        SettingsChange::edit(move |config| config.language = language),
                    );
                },
            );
            let control = select.clone();
            rows.push(layout::row(
                &theme,
                "language",
                label,
                description,
                move |app, window, parent| {
                    control.mount(app, window, parent)?;
                    Ok(())
                },
            ));
            self.language = Some(select);
        }

        layout::section(app, window, parent, &theme, "appearance", None, rows)?;
        self.applied = Some((config.appearance, config.language));

        Ok(())
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        let (appearance, language) = self
            .host
            .config(app, |config| (config.appearance, config.language));
        if self.applied == Some((appearance, language)) {
            return Ok(());
        }

        if let Some(select) = &self.appearance {
            select.set_chosen(app, [appearance_id(appearance)], false);
        }
        if let Some(select) = &self.language {
            select.set_chosen(app, [language.locale()], false);
        }
        self.applied = Some((appearance, language));

        Ok(())
    }

    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        let mut consumed = false;
        if let Some(select) = &self.appearance {
            consumed |= select.dispatch(app, event);
        }
        if let Some(select) = &self.language {
            consumed |= select.dispatch(app, event);
        }
        consumed
    }

    fn release(&mut self, app: &mut App) {
        if let Some(select) = self.appearance.take() {
            select.release(app);
        }
        if let Some(select) = self.language.take() {
            select.release(app);
        }
        self.applied = None;
    }
}

/// The text this page and its settings are searched by.
pub(crate) fn search(translate: &Translate) -> PageSearch {
    let title = i18n::t_smol!(translate, "settings_ui.appearance.title");
    let mode = i18n::t_smol!(translate, "settings_ui.appearance.mode");
    let mode_description = i18n::t_smol!(translate, "settings_ui.appearance.mode_description");
    let language = i18n::t_smol!(translate, "settings_ui.appearance.language");
    let language_description =
        i18n::t_smol!(translate, "settings_ui.appearance.language_description");

    PageSearch {
        label: words(&[
            title.as_ref(),
            "appearance",
            "style",
            "外观",
            "主题",
            "浅色",
            "深色",
        ]),
        fields: vec![
            FieldSearch {
                id: SmolStr::new_static("appearance"),
                text: words(&[
                    mode.as_ref(),
                    mode_description.as_ref(),
                    "appearance",
                    "style",
                    "浅色",
                    "深色",
                    "系统",
                    "主题",
                    "外观",
                ]),
            },
            FieldSearch {
                id: SmolStr::new_static("language"),
                text: words(&[
                    language.as_ref(),
                    language_description.as_ref(),
                    "language",
                    "locale",
                    "语言",
                    "中文",
                    "english",
                    "英文",
                ]),
            },
        ],
    }
}

/// The id the appearance control names one preference by.
fn appearance_id(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Light => "Light",
        Appearance::Dark => "Dark",
        Appearance::System => "System",
    }
}

/// The preference one choice of the appearance control names.
fn appearance_of(value: &str) -> Appearance {
    match value {
        "Light" => Appearance::Light,
        "Dark" => Appearance::Dark,
        _ => Appearance::System,
    }
}

/// The language one choice of the language control names.
fn language_of(value: &str) -> Language {
    match value {
        "en" => Language::En,
        _ => Language::ZhCn,
    }
}

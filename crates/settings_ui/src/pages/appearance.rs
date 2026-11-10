//! The appearance page: the application's theme and the language it is drawn in.

use app_config::settings::{Appearance, Language};
use gpui_kit::{AnyElement, App, IntoElement, SharedString};
use i18n::t;

use crate::{
    layout::{self, PageArgs},
    search::{FieldSearch, PageSearch, words},
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

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    let mut rows = Vec::new();

    if args.filter.admits_field(PAGE, "appearance") {
        let label = t!(args.cx, "settings_ui.appearance.mode");
        let description = t!(args.cx, "settings_ui.appearance.mode_description");
        let selected = SharedString::new_static(appearance_id(args.config.appearance));
        let choices = APPEARANCE_OPTIONS
            .into_iter()
            .map(|(id, label)| (SharedString::new_static(id), t!(args.cx, label)));
        let select = layout::choice_select(
            layout::element_id("settings-select", "appearance"),
            Some(selected),
            choices,
            label.clone(),
            None,
            false,
            layout::on_select(args.handle, |config, value| {
                config.appearance = appearance_of(value);
            }),
        );

        rows.push(layout::row(
            "appearance",
            label,
            description,
            select.into_any_element(),
            layout::CONTROL_WIDTH,
            args.cx,
        ));
    }

    if args.filter.admits_field(PAGE, "language") {
        let label = t!(args.cx, "settings_ui.appearance.language");
        let description = t!(args.cx, "settings_ui.appearance.language_description");
        let selected = SharedString::from(args.config.language.locale());
        let choices = LANGUAGE_OPTIONS
            .into_iter()
            .map(|(id, label)| (SharedString::new_static(id), t!(args.cx, label)));
        let select = layout::choice_select(
            layout::element_id("settings-select", "language"),
            Some(selected),
            choices,
            label.clone(),
            None,
            false,
            layout::on_select(args.handle, |config, value| {
                config.language = language_of(value);
            }),
        );

        rows.push(layout::row(
            "language",
            label,
            description,
            select.into_any_element(),
            layout::CONTROL_WIDTH,
            args.cx,
        ));
    }

    if rows.is_empty() {
        return layout::page(Vec::new());
    }

    layout::page(vec![layout::section(None, "appearance_0", rows, args.cx)])
}

pub(crate) fn search(cx: &App) -> PageSearch {
    let title = t!(cx, "settings_ui.appearance.title");
    let mode = t!(cx, "settings_ui.appearance.mode");
    let mode_description = t!(cx, "settings_ui.appearance.mode_description");
    let language = t!(cx, "settings_ui.appearance.language");
    let language_description = t!(cx, "settings_ui.appearance.language_description");

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
                id: "appearance".into(),
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
                id: "language".into(),
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

fn appearance_id(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Light => "Light",
        Appearance::Dark => "Dark",
        Appearance::System => "System",
    }
}

fn appearance_of(value: &str) -> Appearance {
    match value {
        "Light" => Appearance::Light,
        "Dark" => Appearance::Dark,
        _ => Appearance::System,
    }
}

fn language_of(value: &str) -> Language {
    match value {
        "en" => Language::En,
        _ => Language::ZhCn,
    }
}

//! The translation page: the models this build supports and the directory they are loaded from.
//!
//! The page is a view of its own rather than a page of fields, because its model list is static
//! content and its directory is an input that only the system folder picker writes. The chosen
//! directory is recorded in the session draft, so confirming the dialog is what stores it, and
//! the field reads the directory back from the draft, so a session that is reopened shows the
//! stored one.

use std::path::PathBuf;

use base_ui::{
    button::Button,
    input::{Input, InputState, InputStateBuilder},
    interactive::InteractiveVariantExt,
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement,
    PathPromptOptions, Render, SharedString, StatefulInteractiveElement as _, Styled, Window, div,
};
use i18n::t;

use crate::{
    draft::{SettingsDraft, SettingsEdit},
    page,
};

/// The models this build supports, in the order the page lists them.
pub(crate) const MODELS: [&str; 4] = [
    "Hy-MT2-1.8B-GGUF",
    "Hy-MT2-1.8B-2bit-GGUF",
    "Hy-MT2-1.8B-1.25bit-GGUF",
    "Hy-MT2-7B-GGUF",
];

/// The element id of the directory field, which is also the key of its state.
const DIRECTORY_INPUT_ID: &str = "settings-ai-directory";

/// The translation page.
pub(crate) struct TranslationPage {
    draft: Entity<SettingsDraft>,
    /// The read-only field that shows the directory, built on the first frame that draws it.
    directory: Option<Entity<InputState>>,
}

impl TranslationPage {
    pub(crate) fn new(draft: Entity<SettingsDraft>) -> Self {
        Self {
            draft,
            directory: None,
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
            draft.write(SettingsEdit::TranslationModelDirectory { directory }, cx);
        });
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
        let input = self.directory_input(window, cx);
        let mut models = div().flex().flex_col().gap_0p5();
        for (index, model) in MODELS.iter().enumerate() {
            models = models.child(
                div()
                    .debug_selector(move || format!("SETTINGS_AI_MODEL_{index}"))
                    .text_sm()
                    .text_color(cx.theme().roles.fg.default.hsla)
                    .child(*model),
            );
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
                    .debug_selector(|| "SETTINGS_AI_LLAMA_CPP".into())
                    .child(page::section_title(
                        SharedString::new_static("LLama.cpp"),
                        cx,
                    )),
            )
            .child(
                div()
                    .debug_selector(|| "SETTINGS_AI_MODELS".into())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().roles.fg.default.hsla)
                            .child(t!(cx, "settings_ui.ai.models")),
                    )
                    .child(models),
            )
            .child(
                div()
                    .debug_selector(|| "SETTINGS_AI_DIRECTORY".into())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().roles.fg.default.hsla)
                            .child(t!(cx, "settings_ui.ai.model_directory")),
                    )
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
    }
}

#[cfg(test)]
mod tests {
    use super::MODELS;

    #[test]
    fn the_page_lists_the_supported_models_in_order() {
        assert_eq!(
            MODELS,
            [
                "Hy-MT2-1.8B-GGUF",
                "Hy-MT2-1.8B-2bit-GGUF",
                "Hy-MT2-1.8B-1.25bit-GGUF",
                "Hy-MT2-7B-GGUF",
            ]
        );
    }
}

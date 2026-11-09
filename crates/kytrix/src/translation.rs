//! Wiring for the local translation model.
//!
//! The model is loaded lazily on the first real translation. The directory it is loaded from is
//! read from the settings on every call, so a directory the user changes is used by the next
//! translation without a restart. Startup only proves that the translation crate is linked and
//! accepts the language codes the app uses; it must not pay for a multi-second model load
//! before the first window.

use ai_translator::error::TranslationError;
use app_config_provider::manager::ConfigManager;
use gpui_kit::App;

/// Translates `content` into `target_lang` through the process-wide model.
///
/// The directory comes from the settings rather than from this module, so changing it in the
/// settings dialog takes effect on the next call.
pub(crate) fn translate(
    cx: &App,
    target_lang: &str,
    content: &str,
) -> Result<String, TranslationError> {
    let config = cx.global::<ConfigManager>().read();
    let directory = config.translation.model_directory.as_deref();
    let selected_model = config.translation.selected_model;

    ai_translator::translator::translate(directory, selected_model, target_lang, content)
}

/// Keeps the translation entry point in the binary after checking it is usable.
pub(crate) fn install(cx: &mut App) -> Result<(), TranslationError> {
    // Empty content is answered before the model is touched. The call keeps the whole
    // translation path in the binary without loading the GGUF at startup.
    let _ = translate(cx, "en", "")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use app_config::settings::Config;
    use app_config_provider::manager::ConfigManager;
    use gpui_kit::TestAppContext;

    #[gpui_kit::test]
    fn the_translation_entry_point_accepts_empty_content_without_a_model(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });

        let translated = cx.update(|cx| {
            super::translate(cx, "en", "").expect("empty content must not load the model")
        });

        assert_eq!(translated, "");
    }
}

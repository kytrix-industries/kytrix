//! Wiring for the local translation model.
//!
//! The model is loaded lazily on the first real translation. Startup only proves
//! that the translation crate is linked and accepts the language codes the app
//! uses; it must not pay for a multi-second model load before the first window.

use ai_translator::error::TranslationError;
use gpui_kit::{App, Global};

/// The application's translation entry point.
pub(crate) struct TranslationService;

impl Global for TranslationService {}

/// Translates `content` into `target_lang` through the process-wide model.
pub(crate) fn translate(target_lang: &str, content: &str) -> Result<String, TranslationError> {
    ai_translator::translator::translate(target_lang, content)
}

/// Installs the translation service after checking the entry point is usable.
pub(crate) fn install(cx: &mut App) -> Result<(), TranslationError> {
    // Empty content is answered before the model is touched. The call keeps the
    // whole translation path in the binary without loading the GGUF at startup.
    let _ = translate("en", "")?;
    cx.set_global(TranslationService);

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_translation_entry_point_accepts_empty_content_without_a_model() {
        let translated = super::translate("en", "").expect("empty content must not load the model");

        assert_eq!(translated, "");
    }
}

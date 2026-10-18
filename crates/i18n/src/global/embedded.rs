//! The locale catalogue compiled into the binary.
//!
//! The messages are embedded rather than read from disk so a packaged build
//! cannot start without the text it renders.

use gpui_kit::App;

use crate::{error::types::Error, language::codes::LanguageCode, translator::types::I18n};

/// Builds a translator from the locale files embedded in this crate.
///
/// # Errors
///
/// Fails when an embedded file is not a valid locale document or when a message
/// is not a valid template.
pub fn load() -> Result<I18n, Error> {
    I18n::builder()
        .locale_yaml(LanguageCode::En, include_str!("../../locales/en.yaml"))?
        .locale_yaml(LanguageCode::ZhCn, include_str!("../../locales/zh-CN.yaml"))?
        .fallback(LanguageCode::En)
        .build()
}

/// Loads the embedded catalogue and installs it in GPUI global state.
///
/// The language is installed next to the catalogue, so a translation macro has
/// both the messages and the language it renders them in. The catalogue comes
/// first: installing it notifies global observers, and one of them would find
/// no messages to render with if the language were installed before it.
///
/// # Errors
///
/// See [`load`].
///
/// # Panics
///
/// Asserts in debug and test builds that no translator is installed yet: the
/// composition root installs exactly one, and a second install would mean two
/// parts of the program disagree about where messages come from. It would also
/// reset the language to [`LanguageCode::En`], discarding the language the user
/// had selected.
///
/// The assertion is a `debug_assert!`, so a release build compiles it out and a
/// second install silently overwrites both the catalogue and the language.
pub fn install(cx: &mut App) -> Result<(), Error> {
    debug_assert!(
        cx.try_global::<I18n>().is_none(),
        "a translator is already installed: a second install would clobber the \
         catalogue and reset the language to En"
    );
    cx.set_global(load()?);
    cx.set_global(LanguageCode::En);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::load;
    use crate::{language::codes::LanguageCode, translator::types::I18n};

    fn keys_of(i18n: &I18n, language: LanguageCode) -> Vec<&str> {
        let mut keys = i18n
            .keys(language)
            .expect("an embedded language should expose its keys")
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn embedded_catalogue_should_load_every_language() {
        let i18n = load().expect("embedded locales should load");

        assert!(i18n.has_language(LanguageCode::En));
        assert!(i18n.has_language(LanguageCode::ZhCn));
        assert_eq!(i18n.languages().len(), 2);
    }

    #[test]
    fn every_language_should_define_exactly_the_same_messages() {
        let i18n = load().expect("embedded locales should load");
        let reference = keys_of(&i18n, LanguageCode::En);
        assert!(!reference.is_empty(), "the catalogue should not be empty");

        for language in i18n.languages() {
            let keys = keys_of(&i18n, language);
            let missing = reference
                .iter()
                .filter(|key| !keys.contains(key))
                .collect::<Vec<_>>();
            let unexpected = keys
                .iter()
                .filter(|key| !reference.contains(key))
                .collect::<Vec<_>>();

            assert!(
                missing.is_empty() && unexpected.is_empty(),
                "{language} is missing {missing:?} and defines unexpected {unexpected:?}"
            );
        }
    }

    #[test]
    fn every_message_should_render_in_every_language() {
        let i18n = load().expect("embedded locales should load");

        for language in i18n.languages() {
            for key in keys_of(&i18n, language) {
                assert!(
                    !i18n.translate(language, key).is_empty(),
                    "{key} renders empty in {language}"
                );
            }
        }
    }
}

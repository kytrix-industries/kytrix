//! The projection of the configured language onto the translator and the engine signal.
//!
//! The language is one stored preference with two consumers: [`Translate`] renders the
//! catalogue text, and [`Signal<LanguageCode>`] is the engine dependency a drawn node
//! registers when its text must follow the language. The two are written together by the
//! composition root — translator first, signal second — because a node that reads the new
//! language from the signal must find the translator already rendering it.
//!
//! [`Signal<LanguageCode>`]: ui_core::node::signal::Signal

use app_config::settings::Language;
use i18n::language::codes::LanguageCode;

/// The translator language a stored language preference names.
pub(crate) const fn code(language: Language) -> LanguageCode {
    match language {
        Language::ZhCn => LanguageCode::ZhCn,
        Language::En => LanguageCode::En,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every stored language has exactly one translator language, and every translator
    /// language has exactly one stored preference: the projection is a bijection, so a
    /// settings write can never name a language the translator does not have.
    #[test]
    fn the_projection_should_cover_every_language_on_both_sides() {
        assert_eq!(Language::ALL.len(), 2);
        for language in Language::ALL {
            let registered = code(language);
            assert_eq!(
                registered,
                match language {
                    Language::ZhCn => LanguageCode::ZhCn,
                    Language::En => LanguageCode::En,
                }
            );
        }
    }
}

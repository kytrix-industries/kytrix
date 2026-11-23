use app_config::settings::Language;
use i18n::language::codes::LanguageCode;

pub(crate) const fn code(language: Language) -> LanguageCode {
    match language {
        Language::ZhCn => LanguageCode::ZhCn,
        Language::En => LanguageCode::En,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

//! The application language projected onto the venue's content locale.
//!
//! The connector treats a locale as opaque request data. The policy of which
//! locale the current application language asks for lives here, in the UI layer.

use app_config::settings::Language;
use smol_str::SmolStr;

/// Returns the Polymarket content locale for one application language.
pub fn polymarket_locale(language: Language) -> Option<SmolStr> {
    match language {
        Language::En => None,
        Language::ZhCn => Some(SmolStr::new_static("zh")),
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::Language;

    use super::polymarket_locale;

    #[test]
    fn the_application_language_maps_to_the_polymarket_locale() {
        assert_eq!(polymarket_locale(Language::En), None);
        assert_eq!(polymarket_locale(Language::ZhCn).as_deref(), Some("zh"));
    }
}

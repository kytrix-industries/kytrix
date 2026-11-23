use app_config::settings::Appearance;
use ui_kit::theme::{appearance::Appearance as ThemeAppearance, handle::ThemeHandle};

pub fn apply(
    preference: Appearance,
    system_dark: bool,
    app: &mut ui_core::app::App,
    theme: &ThemeHandle,
) -> bool {
    theme.set_appearance(app, appearance_of(preference, system_dark))
}

#[must_use]
pub fn appearance_of(preference: Appearance, system_dark: bool) -> ThemeAppearance {
    match preference {
        Appearance::Light => ThemeAppearance::Light,
        Appearance::Dark => ThemeAppearance::Dark,
        Appearance::System => {
            if system_dark {
                ThemeAppearance::Dark
            } else {
                ThemeAppearance::Light
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::Appearance;
    use ui_kit::theme::appearance::Appearance as ThemeAppearance;

    use super::appearance_of;

    #[test]
    fn a_preference_should_defer_to_the_system_only_when_it_names_none() {
        let cases = [
            (Appearance::Light, false, ThemeAppearance::Light),
            (Appearance::Light, true, ThemeAppearance::Light),
            (Appearance::Dark, false, ThemeAppearance::Dark),
            (Appearance::Dark, true, ThemeAppearance::Dark),
            (Appearance::System, false, ThemeAppearance::Light),
            (Appearance::System, true, ThemeAppearance::Dark),
        ];

        for (preference, system_dark, expected) in cases {
            assert_eq!(
                appearance_of(preference, system_dark),
                expected,
                "{preference:?} on a system reporting dark = {system_dark}"
            );
        }
    }
}

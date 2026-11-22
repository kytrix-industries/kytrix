//! The appearance the application is drawn in, and the precedence it is resolved by.
//!
//! The persisted preference and the appearance the operating system reports are two things, and
//! what the interface is drawn in is decided between them: an appearance the user chose is what
//! they asked for and outranks the system, so `Light` and `Dark` pin the theme, and `System` is the
//! preference that defers to the operating system.
//!
//! Nothing here owns the theme: [`apply`] resolves the precedence and hands the appearance it
//! resolved to the injected [`ThemeHandle`], which is where the colors live.

use app_config::settings::Appearance;
use base_ui::theme::{appearance::Appearance as ThemeAppearance, handle::ThemeHandle};

/// Draws the application the way `preference` asks, through `theme`.
///
/// `System` is resolved against the appearance the operating system reports for the window the
/// theme is drawn into (`system_dark`); a preference that names an appearance is taken as it is.
/// Returns whether the theme moved.
///
/// See the module documentation for why the preference outranks the system.
pub fn apply(
    preference: Appearance,
    system_dark: bool,
    app: &mut ui_core::app::App,
    theme: &ThemeHandle,
) -> bool {
    theme.set_appearance(app, appearance_of(preference, system_dark))
}

/// The appearance a preference resolves to, against the appearance the system reports.
///
/// Both of the things the precedence is decided between are arguments, so it is decided against
/// them rather than against the window the caller holds, and a test can put a system appearance to
/// it that the machine running the test does not have.
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
    use base_ui::theme::appearance::Appearance as ThemeAppearance;

    use super::appearance_of;

    /// The whole precedence: every preference against both appearances a system reports.
    ///
    /// It is a table because its two halves fail differently. A preference that names an appearance
    /// and read the system anyway would pin nothing, and a `System` that answered a fixed
    /// appearance would follow nothing — and only the second of those is invisible to a test that
    /// runs on the one system appearance its own machine happens to report, which is why the system
    /// is an argument here rather than the window's.
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

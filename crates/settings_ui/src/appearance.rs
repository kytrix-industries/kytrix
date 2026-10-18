//! The appearance the application is drawn in, and the precedence it is resolved by.
//!
//! The persisted preference and the appearance the operating system reports are two things, and
//! what the interface is drawn in is decided between them: an appearance the user chose is what
//! they asked for and outranks the system, so `Light` and `Dark` pin the theme, and `System` is
//! the preference that defers to the operating system. System changes are watched only while
//! they are what is being followed — the window holds the observer that watches them, and it
//! re-reads the preference from the settings each time one arrives.
//!
//! Nothing here owns the theme: [`apply`] resolves the precedence and hands the appearance it
//! resolved to `base_ui`'s theme, which is where the colors live.

use app_config::settings::Appearance;
use base_ui::style::{palette::Appearance as ThemeAppearance, theme};
use gpui_kit::{App, Window, WindowAppearance};

/// Draws the application the way `preference` asks, in `window`.
///
/// `System` is resolved against the appearance this window's operating system reports, which is
/// the appearance the window is drawn with natively; a preference that names an appearance is
/// taken as it is. See the module documentation for why the preference outranks the system.
///
/// # Panics
///
/// Panics when no theme was installed, which is what reading a global GPUI does not hold does. A
/// theme is installed before the first window is built and it is a window that resolves an
/// appearance, so a caller inside a running application has one; see `base_ui`'s
/// [`theme::install`].
pub fn apply(preference: Appearance, window: &Window, cx: &mut App) {
    theme::apply(appearance_of(preference, window.appearance()), cx);
}

/// The appearance a preference resolves to, against the appearance the system reports.
///
/// Both of the things the precedence is decided between are arguments, so it is decided against
/// them rather than against the window the caller holds, and a test can put a system appearance to
/// it that the machine running the test does not have.
fn appearance_of(preference: Appearance, system: WindowAppearance) -> ThemeAppearance {
    match preference {
        Appearance::Light => ThemeAppearance::Light,
        Appearance::Dark => ThemeAppearance::Dark,
        Appearance::System => match system {
            // The vibrant appearances are the same lightness as their plain counterparts, with the
            // window's own material behind them; an application that draws its own background
            // reads them as the light and dark they are.
            WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeAppearance::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeAppearance::Dark,
        },
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::Appearance;
    use base_ui::style::palette::Appearance as ThemeAppearance;
    use gpui_kit::WindowAppearance;

    use super::appearance_of;

    /// The whole precedence: every preference against every appearance a system reports.
    ///
    /// It is a table because its two halves fail differently. A preference that names an appearance
    /// and read the system anyway would pin nothing, and a `System` that answered a fixed appearance
    /// would follow nothing — and only the second of those is invisible to a test that runs on the
    /// one system appearance its own machine happens to report, which is why the system is an
    /// argument here rather than the window's.
    #[test]
    fn a_preference_should_defer_to_the_system_only_when_it_names_none() {
        let cases = [
            (
                Appearance::Light,
                WindowAppearance::Light,
                ThemeAppearance::Light,
            ),
            (
                Appearance::Light,
                WindowAppearance::Dark,
                ThemeAppearance::Light,
            ),
            (
                Appearance::Dark,
                WindowAppearance::Light,
                ThemeAppearance::Dark,
            ),
            (
                Appearance::Dark,
                WindowAppearance::Dark,
                ThemeAppearance::Dark,
            ),
            (
                Appearance::System,
                WindowAppearance::Light,
                ThemeAppearance::Light,
            ),
            (
                Appearance::System,
                WindowAppearance::VibrantLight,
                ThemeAppearance::Light,
            ),
            (
                Appearance::System,
                WindowAppearance::Dark,
                ThemeAppearance::Dark,
            ),
            (
                Appearance::System,
                WindowAppearance::VibrantDark,
                ThemeAppearance::Dark,
            ),
        ];

        for (preference, system, expected) in cases {
            assert_eq!(
                appearance_of(preference, system),
                expected,
                "{preference:?} on a system reporting {system:?}"
            );
        }
    }
}

//! The interface's colors, as one GPUI global.
//!
//! The global holds a whole [`Theme`]: the appearance it is drawn in, the palette that appearance
//! is authored with, the roles resolved from it and the component colors resolved from those. The
//! four are one value because they are one decision — a reader never sees roles that disagree with
//! their palette or with the components drawn from them.
//!
//! Which appearance the application is drawn in is not decided here. The persisted preference, the
//! appearance the operating system reports and the precedence between them belong to the settings
//! layer that owns both; what it hands this module is the appearance it resolved them to.
//!
//! [`install`] puts the first theme in place and [`apply`] is what moves the global after that.

use gpui_kit::{App, Global};

use crate::style::{
    components::ComponentColors,
    palette::{Appearance, Palette},
    roles::Roles,
};

/// Installs the light theme.
///
/// This is what a window that decides nothing about its appearance installs: an example and a test
/// draw in the light appearance, and the composition root replaces it with the appearance the
/// settings resolve to before the window draws its first frame. See [`apply`].
pub fn install(cx: &mut App) {
    cx.set_global(Theme::of(Appearance::default()));
}

/// Draws the application in `appearance`, and asks every window to draw again.
///
/// The windows are asked because a GPUI global is not a first-class observable: setting one
/// notifies the observers registered for its type, it does not mark any window as needing a frame.
/// Without the refresh the interface would keep the colors of the previous appearance until
/// something else happened to dirty the window — see `kytrix`'s language projection, which
/// refreshes for the same reason after `LanguageChanged` moves the language global.
///
/// An appearance the application is already drawn in is not a change: it neither replaces the
/// global nor redraws, so resolving the appearance twice — a window built after a dialog confirmed
/// one, an operating system that reports the appearance that was already in use — costs nothing.
///
/// # Panics
///
/// Panics when no theme was installed: reading a global that GPUI does not hold panics, and
/// [`apply`] reads the current theme to decide whether the appearance changed. [`install`] is the
/// one thing that has to have run before this: a window that resolves an appearance has a theme to
/// resolve it into.
pub fn apply(appearance: Appearance, cx: &mut App) {
    if cx.theme().appearance == appearance {
        return;
    }

    cx.set_global(Theme::of(appearance));
    cx.refresh_windows();
}

/// One appearance, loaded: the palette it is authored with and everything derived from it.
pub struct Theme {
    /// The appearance the theme draws.
    pub appearance: Appearance,
    /// The hand-written values it is authored with.
    pub palette: Palette,
    /// The semantic roles its components read.
    pub roles: Roles,
    /// The component recipes resolved from those roles.
    pub components: ComponentColors,
}

impl Theme {
    /// Loads the theme one appearance is drawn with.
    ///
    /// The resolve is the whole cost of a theme: about forty roles mixed and converted, then the
    /// recipes combined from them. It happens once per appearance switch; a frame after it reads
    /// the resolved values and converts nothing.
    pub fn of(appearance: Appearance) -> Self {
        let palette = match appearance {
            Appearance::Light => Palette::light(),
            Appearance::Dark => Palette::dark(),
        };
        let roles = Roles::resolve(&palette);

        Self {
            components: ComponentColors::resolve(&roles),
            roles,
            palette,
            appearance,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::of(Appearance::default())
    }
}

impl Global for Theme {}

pub trait ThemeExt {
    fn theme(&self) -> &Theme;
}

impl ThemeExt for App {
    fn theme(&self) -> &Theme {
        self.global()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use super::{Theme, apply, install};
    use crate::style::palette::Appearance;

    #[gpui_kit::test]
    fn install_should_draw_in_the_light_appearance(cx: &mut TestAppContext) {
        cx.update(install);

        assert_eq!(
            cx.read_global(|theme: &Theme, _| theme.appearance),
            Appearance::Light,
            "an example or a test draws in the light appearance unless it says otherwise"
        );
    }

    #[gpui_kit::test]
    fn apply_should_replace_the_theme_when_the_appearance_changes(cx: &mut TestAppContext) {
        cx.update(install);
        let light = cx.read_global(|theme: &Theme, _| theme.roles.selection);

        cx.update(|cx| apply(Appearance::Dark, cx));

        let (appearance, selection) =
            cx.read_global(|theme: &Theme, _| (theme.appearance, theme.roles.selection));
        assert_eq!(appearance, Appearance::Dark);
        assert_ne!(
            selection, light,
            "a selection drawn in the dark appearance is a different color"
        );
    }

    /// A theme is one whole: the roles in it and the component colors in it come from the same
    /// palette, never from the palette that was there before.
    ///
    /// The text selection is the probe because it is the one paint that is not a palette value:
    /// both the role and the input that paints with it are derived from the palette, so a theme
    /// whose appearance was replaced while its paints were not reports the new appearance and
    /// still selects text with the old highlight.
    #[gpui_kit::test]
    fn a_theme_should_carry_the_roles_and_components_of_the_appearance_it_names(
        cx: &mut TestAppContext,
    ) {
        cx.update(install);
        cx.update(|cx| apply(Appearance::Dark, cx));

        let (appearance, selection, input_selection) = cx.read_global(|theme: &Theme, _| {
            (
                theme.appearance,
                theme.roles.selection,
                theme.components.input.selection,
            )
        });
        let dark = Theme::of(Appearance::Dark);

        assert_eq!(appearance, Appearance::Dark);
        assert_eq!(
            selection, dark.roles.selection,
            "the roles in the global are the ones the dark palette resolves to"
        );
        assert_eq!(
            input_selection, selection,
            "the component colors in the global are resolved from the roles in the global"
        );
        assert_ne!(
            selection,
            Theme::of(Appearance::Light).roles.selection,
            "and the dark selection is not the light one"
        );
        // §7.4 spells the dark selection out: the brand mixed 62% of the way into the page is
        // OKLCH L 0.408, C 0.059. A theme that selects text with the light appearance's highlight
        // passes the equality above only if the roles were rebuilt too, so the value is the
        // probe that both halves of the theme moved together.
        assert!(
            (selection.oklch.lightness() - 0.408).abs() < 0.002
                && (selection.oklch.chroma() - 0.059).abs() < 0.002,
            "the dark selection reads L {:.3}, C {:.3}, where §7.4 documents L 0.408, C 0.059",
            selection.oklch.lightness(),
            selection.oklch.chroma()
        );
    }

    /// Switching to the appearance already in use changes nothing and does not mark a window.
    #[gpui_kit::test]
    fn apply_should_leave_the_theme_alone_when_the_appearance_is_the_same(cx: &mut TestAppContext) {
        cx.update(install);
        let before = cx.read_global(|theme: &Theme, _| theme.roles.selection);

        cx.update(|cx| apply(Appearance::Light, cx));

        assert_eq!(
            cx.read_global(|theme: &Theme, _| theme.roles.selection),
            before,
            "resolving the appearance already in use must not rebuild the theme"
        );
    }
}

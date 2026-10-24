//! A small label that says what something is.
//!
//! A badge has two choices: a color — neutral, or one of the four status colors — and a shape. The
//! shape is how the color is worn: a *tonal* badge is a status tint with the status color written
//! on it (the control surface under body text, for a neutral one), a *solid* badge is a block — the
//! inverse of the page for a neutral one, the saturated status color for a status one — and an
//! *outline* badge is a hairline with no fill at all (§8.5).
//!
//! The two choices have different defaults, which is why the shape is `Option`: a status badge is
//! tonal unless it says otherwise, because a status is a tint first, and a neutral badge is solid,
//! because a neutral label is a block rather than a message. That is also the one visible change
//! this model makes to the default badge: it used to be filled with the brand color, which read as
//! a black chip in one appearance and a blue one in the other; it is now the inverse of the page in
//! both.

use gpui_kit::{AnyElement, App, IntoElement, ParentElement, RenderOnce, Styled, Window, div};
use smallvec::SmallVec;

use crate::{
    size::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle},
    style::{
        color::ResolvedColor,
        roles::{Roles, StatusRole},
        theme::ThemeExt,
    },
};

#[derive(Default, Copy, Clone, Eq, PartialEq)]
enum BadgeColor {
    #[default]
    Neutral,
    Info,
    Success,
    Warning,
    Elevated,
    Error,
}

/// How a badge's color is worn (§8.5).
#[derive(Copy, Clone, Eq, PartialEq)]
enum BadgeShape {
    /// A status tint, with the status color written on it.
    Tonal,
    /// A block: the inverse of the page for a neutral badge, the saturated status color for a
    /// status badge.
    Solid,
    /// A hairline with no fill.
    Outline,
}

#[derive(IntoElement)]
pub struct Badge {
    unified_size: UnifiedSize,
    color: BadgeColor,
    shape: Option<BadgeShape>,
    children: SmallVec<[AnyElement; 2]>,
}

impl UnifiedSizable for Badge {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = size;
        self
    }
}

impl Badge {
    pub fn new() -> Self {
        Self {
            unified_size: UnifiedSize::Medium,
            color: BadgeColor::default(),
            shape: None,
            children: SmallVec::new(),
        }
    }

    /// Wears the badge's color as a tint: the status surface under the status color.
    pub fn tonal(mut self) -> Self {
        self.shape = Some(BadgeShape::Tonal);
        self
    }

    /// Wears the badge's color as a solid block: the inverse of the page, for neutral, and the
    /// saturated status color for a status badge.
    pub fn solid(mut self) -> Self {
        self.shape = Some(BadgeShape::Solid);
        self
    }

    /// Wears the badge's color as a hairline with no fill.
    pub fn outline(mut self) -> Self {
        self.shape = Some(BadgeShape::Outline);
        self
    }

    pub fn default(mut self) -> Self {
        self.color = BadgeColor::Neutral;
        self
    }

    pub fn info(mut self) -> Self {
        self.color = BadgeColor::Info;
        self
    }

    pub fn success(mut self) -> Self {
        self.color = BadgeColor::Success;
        self
    }

    pub fn warning(mut self) -> Self {
        self.color = BadgeColor::Warning;
        self
    }

    /// The rung between `warning` and `error`: a reading that is no longer merely high.
    pub fn elevated(mut self) -> Self {
        self.color = BadgeColor::Elevated;
        self
    }

    pub fn error(mut self) -> Self {
        self.color = BadgeColor::Error;
        self
    }
}

impl Default for Badge {
    fn default() -> Self {
        Self::new()
    }
}

impl ParentElement for Badge {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Badge {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Badge {
            unified_size,
            color,
            shape,
            children,
        } = self;

        let roles = &cx.theme().roles;
        let shape = shape.unwrap_or(match color {
            BadgeColor::Neutral => BadgeShape::Solid,
            BadgeColor::Info
            | BadgeColor::Success
            | BadgeColor::Warning
            | BadgeColor::Elevated
            | BadgeColor::Error => BadgeShape::Tonal,
        });

        let (background, foreground, border) = match color {
            BadgeColor::Neutral => neutral_paints(roles, shape),
            BadgeColor::Info => status_paints(roles, roles.status.info, shape),
            BadgeColor::Success => status_paints(roles, roles.status.success, shape),
            BadgeColor::Warning => status_paints(roles, roles.status.warning, shape),
            BadgeColor::Elevated => status_paints(roles, roles.status.elevated, shape),
            BadgeColor::Error => status_paints(roles, roles.status.danger, shape),
        };

        div()
            .unified_sized(unified_size)
            .flex()
            .items_center()
            .justify_center()
            .bg(background.hsla)
            .border_1()
            .border_color(border.hsla)
            .text_color(foreground.hsla)
            .rounded_sm()
            .children(children)
    }
}

/// The paints of a neutral badge: a page-inverse block, a soft neutral chip, or a hairline.
fn neutral_paints(
    roles: &Roles,
    shape: BadgeShape,
) -> (ResolvedColor, ResolvedColor, ResolvedColor) {
    match shape {
        BadgeShape::Tonal => (roles.surface.control, roles.fg.default, roles.line.border),
        BadgeShape::Solid => (
            roles.surface.inverse,
            roles.fg.on_inverse,
            roles.surface.inverse,
        ),
        BadgeShape::Outline => (
            ResolvedColor::transparent(),
            roles.fg.default,
            roles.line.divider,
        ),
    }
}

/// The paints of a status badge: a status tint, a saturated block, or a hairline.
///
/// The solid form is the pairing §8.1 gives a destructive filled button — the status color under
/// the text that is read on a saturated fill — so a solid status badge needs no color the theme
/// does not already carry.
fn status_paints(
    roles: &Roles,
    status: StatusRole,
    shape: BadgeShape,
) -> (ResolvedColor, ResolvedColor, ResolvedColor) {
    match shape {
        BadgeShape::Tonal => (status.surface, status.base, status.border),
        BadgeShape::Solid => (status.base, roles.fg.on_brand, status.base),
        BadgeShape::Outline => (ResolvedColor::transparent(), status.base, status.border),
    }
}

#[cfg(test)]
mod tests {
    use super::{BadgeShape, status_paints};
    use crate::style::{
        palette::Palette,
        roles::{Roles, contrast},
    };

    /// A solid status badge is §7.3's "text on a saturated fill" pairing applied to the four
    /// status colors: `fg.on_brand` on the status itself. §7.8 gives that pairing a row for
    /// `brand` only, so the four status fills are checked here, where the badge is what introduces
    /// them; the weakest pair in either appearance is warning in the light one.
    #[test]
    fn a_solid_status_badge_should_read_its_text_on_the_saturated_fill() {
        for palette in [Palette::light(), Palette::dark()] {
            let roles = Roles::resolve(&palette);

            for (name, status) in [
                ("danger", roles.status.danger),
                ("warning", roles.status.warning),
                ("success", roles.status.success),
                ("info", roles.status.info),
                ("elevated", roles.status.elevated),
            ] {
                let (background, foreground, _) = status_paints(&roles, status, BadgeShape::Solid);
                let ratio = contrast(foreground, background);

                assert!(
                    ratio >= 4.5,
                    "{:?}: a solid {name} badge reads {ratio:.2}:1 against its fill, where text \
                     needs 4.5:1",
                    palette.appearance
                );
            }
        }
    }
}

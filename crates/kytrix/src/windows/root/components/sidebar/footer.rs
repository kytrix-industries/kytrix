//! The foot of the sidebar: who is signed in, and the way into the settings.
//!
//! Both entries are the same row, drawn here: the account's face and the gear are marks of
//! one size, and each is the trigger of the menu or dialog it opens. The account menu's
//! content belongs to `profile_ui`; the row that opens it is the sidebar's.

use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
};
use gpui_kit::{
    App, Bounds, ElementId, Entity, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, base::ElementExt as _, div, point,
    px,
};
use i18n::t;
use profile_ui::profile::ProfileMenu;
use settings_ui::entry::show_settings;

use super::layout::{SIDEBAR_FULL_PADDING, SIDEBAR_ICON_SIZE, SIDEBAR_ITEM_SIZE, SIDEBAR_PADDING};

/// The account entry, named whether or not it draws its label.
pub(crate) const SIDEBAR_FOOTER_PROFILE: &str = "SIDEBAR_FOOTER_PROFILE";

/// The account label, drawn beside the avatar in the full sidebar.
pub(crate) const SIDEBAR_FOOTER_PROFILE_LABEL: &str = "SIDEBAR_FOOTER_PROFILE_LABEL";

/// The settings button.
pub(crate) const SIDEBAR_FOOTER_SETTINGS: &str = "SIDEBAR_FOOTER_SETTINGS";

/// The settings label, drawn beside the gear in the full sidebar.
pub(crate) const SIDEBAR_FOOTER_SETTINGS_LABEL: &str = "SIDEBAR_FOOTER_SETTINGS_LABEL";

/// The ids the account entry takes in each of the two panels that can be drawn at once.
///
/// The icon-only rail and the full panel it floats are both in the tree while the preview
/// is up: they need separate element and keyed state ids. The full panel appends the
/// opening's scope, so the entry's ids do not survive the panel it belongs to.
const PROFILE_MENU_FULL: &str = "sidebar-profile-full";
const PROFILE_MENU_ICON_ONLY: &str = "sidebar-profile-icon-only";

/// The account and the settings button at the bottom of the sidebar.
#[derive(IntoElement)]
pub(crate) struct SidebarFooter {
    /// Whether it draws the labelled panel rather than the icon-only rail.
    full: bool,
    profile: Entity<ProfileMenu>,
    /// Identifies one opening of the panel, so the account entry's ids do not outlive it.
    popup_scope: u64,
}

impl SidebarFooter {
    pub(crate) fn new(full: bool, profile: Entity<ProfileMenu>) -> Self {
        Self {
            full,
            profile,
            popup_scope: 0,
        }
    }

    /// Names this opening of the panel in the account menu's id.
    pub(crate) fn popup_scope(mut self, scope: u64) -> Self {
        self.popup_scope = scope;
        self
    }
}

impl RenderOnce for SidebarFooter {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let full = self.full;
        let settings = t!(cx, "window.sidebar.settings");

        // The account entry is the sidebar's own row: both entries in this foot are the same
        // mark-and-label row, and only the menu behind the entry belongs to the profile menu.
        // The face is drawn at a mark's size, because a larger face centred in the icon-only
        // square would jump right when the row expands; at this size it keeps the same
        // eight-pixel inset in both.
        let avatar_profile = self.profile.clone();
        let click_profile = self.profile;
        // The panel's opening names the full panel's menu, so a menu left open when the
        // floating panel went away is not brought back with the next opening.
        let profile_id = if full {
            ElementId::NamedInteger(
                SharedString::new_static(PROFILE_MENU_FULL),
                self.popup_scope,
            )
        } else {
            ElementId::from(PROFILE_MENU_ICON_ONLY)
        };
        let profile_bounds =
            window.use_keyed_state((profile_id.clone(), "bounds"), cx, |_, _| Bounds::default());

        let user = t!(cx, "window.sidebar.user");
        let mut entry = Button::new(SIDEBAR_FOOTER_PROFILE)
            .ghost()
            .h(SIDEBAR_ITEM_SIZE)
            .debug_selector(|| SIDEBAR_FOOTER_PROFILE.into())
            .aria_label(user.clone())
            .child(avatar_profile.read(cx).avatar(SIDEBAR_ICON_SIZE, cx));
        entry = if full {
            entry.w_full().px(SIDEBAR_FULL_PADDING).gap_2().child(
                div()
                    .debug_selector(|| SIDEBAR_FOOTER_PROFILE_LABEL.into())
                    // The panel narrows while it collapses, and the label gives way to the
                    // mark rather than reaching past the panel's edge.
                    .min_w_0()
                    .truncate()
                    .child(user),
            )
        } else {
            entry.w(SIDEBAR_ITEM_SIZE).justify_center()
        };
        entry = entry.on_click({
            let profile_id = profile_id.clone();
            let profile_bounds = profile_bounds.clone();
            move |_, window, cx| {
                let bounds = *profile_bounds.read(cx);
                let anchor = point(bounds.right() + px(4.0), bounds.bottom());

                click_profile.update(cx, |profile, cx| {
                    profile.show_context_menu(profile_id.clone(), anchor, window, cx);
                });
            }
        });

        // The row's own bounds are what the menu anchors to: its bottom-left sits at the
        // row's bottom-right, so the account menu grows upward from the foot of the panel.
        let profile_entry = div()
            .id((profile_id, "row"))
            .on_prepaint(move |bounds, _, cx| *profile_bounds.as_mut(cx) = bounds)
            .child(entry);
        let profile_entry = if full {
            profile_entry.w_full()
        } else {
            profile_entry.w(SIDEBAR_ITEM_SIZE)
        };

        let mut settings_button = Button::new(SIDEBAR_FOOTER_SETTINGS)
            .ghost()
            .h(SIDEBAR_ITEM_SIZE)
            .debug_selector(|| SIDEBAR_FOOTER_SETTINGS.into())
            .aria_label(settings.clone())
            // The mark keeps its size while the panel narrows: a mark that shrinks would
            // slide into the label beside it on every frame of the movement.
            .child(
                Icon::new(IconKind::GearSix)
                    .size(SIDEBAR_ICON_SIZE)
                    .flex_none(),
            )
            .on_click(|_, window, cx| show_settings(window, cx));
        settings_button = if full {
            settings_button
                .w_full()
                .px(SIDEBAR_FULL_PADDING)
                .gap_2()
                .child(
                    div()
                        .debug_selector(|| SIDEBAR_FOOTER_SETTINGS_LABEL.into())
                        // As above: a narrowing panel takes the label away before it can
                        // reach past the panel's edge.
                        .min_w_0()
                        .truncate()
                        .child(settings),
                )
        } else {
            // The gear keeps the column's square, like the workspaces above it. Its name is
            // read from the full panel the pointer floats over the rail, not from a tooltip.
            settings_button.w(SIDEBAR_ITEM_SIZE).justify_center()
        };

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p(SIDEBAR_PADDING)
            .child(profile_entry)
            .child(settings_button)
    }
}

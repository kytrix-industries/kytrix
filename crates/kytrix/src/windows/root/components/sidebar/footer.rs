//! The foot of the sidebar: the way into the settings.

use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
};
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, Window, div,
};
use i18n::t;

use super::layout::{SIDEBAR_FULL_PADDING, SIDEBAR_ICON_SIZE, SIDEBAR_ITEM_SIZE, SIDEBAR_PADDING};
use crate::windows::root::components::pages::MainPages;

/// The settings button.
pub(crate) const SIDEBAR_FOOTER_SETTINGS: &str = "SIDEBAR_FOOTER_SETTINGS";

/// The settings label, drawn beside the gear in the full sidebar.
pub(crate) const SIDEBAR_FOOTER_SETTINGS_LABEL: &str = "SIDEBAR_FOOTER_SETTINGS_LABEL";

/// The settings button at the bottom of the sidebar.
#[derive(IntoElement)]
pub(crate) struct SidebarFooter {
    /// Whether it draws the labelled panel rather than the icon-only rail.
    full: bool,
    /// The page area the settings entry switches to the settings page.
    pages: Entity<MainPages>,
}

impl SidebarFooter {
    pub(crate) fn new(full: bool, pages: Entity<MainPages>) -> Self {
        Self { full, pages }
    }
}

impl RenderOnce for SidebarFooter {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let full = self.full;
        let settings = t!(cx, "window.sidebar.settings");

        let pages = self.pages;
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
            .on_click(move |_, window, cx| {
                pages.update(cx, |pages, cx| pages.show_settings(window, cx));
            });
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
            .p(SIDEBAR_PADDING)
            .child(settings_button)
    }
}

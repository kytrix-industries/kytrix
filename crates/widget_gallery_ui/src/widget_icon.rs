//! The icon of one widget in the gallery.
//!
//! A widget declares an asset path rather than a picture, so the path is resolved where
//! the icon is drawn. The declared icon is a picture and is drawn through the image
//! pipeline — the same one the application's other artwork goes through — so any format
//! that pipeline decodes is a valid declaration. A widget that declares no icon gets the
//! gallery's own, so a widget that has not been given a picture yet is still a picture
//! beside its name rather than a gap.
//!
//! The size is the caller's: the same icon is drawn at the size the row has room for,
//! which is what keeps this element out of the decisions about where it stands.

use base_ui::icon::{Icon, IconKind};
use gpui_kit::{
    App, InteractiveElement as _, IntoElement, ParentElement, Pixels, RenderOnce, Styled, Window,
    div, img,
};

/// Debug selectors of an icon that was declared by the widget, and of the gallery's own.
const DECLARED_ICON_SELECTOR: &str = "WIDGET_GALLERY_ICON_DECLARED";
const FALLBACK_ICON_SELECTOR: &str = "WIDGET_GALLERY_ICON_FALLBACK";

#[derive(IntoElement)]
pub(super) struct WidgetIcon {
    icon: Option<&'static str>,
    size: Pixels,
}

impl WidgetIcon {
    pub(super) fn new(icon: Option<&'static str>, size: Pixels) -> Self {
        Self { icon, size }
    }
}

impl RenderOnce for WidgetIcon {
    fn render(self, _window: &mut Window, _: &mut App) -> impl IntoElement {
        let fallback = self.icon.is_none();
        let icon = match self.icon {
            Some(path) => img(path).size(self.size).into_any_element(),
            None => Icon::new(IconKind::PuzzlePiece)
                .size(self.size)
                .into_any_element(),
        };

        div()
            .debug_selector(move || {
                String::from(if fallback {
                    FALLBACK_ICON_SELECTOR
                } else {
                    DECLARED_ICON_SELECTOR
                })
            })
            .flex_none()
            .child(icon)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        Context, IntoElement, ParentElement, Render, Styled as _, TestAppContext, Window, div, px,
    };

    use super::WidgetIcon;

    /// A window holding one icon of each kind, drawn at the size a row gives them.
    struct Icons;

    impl Render for Icons {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .child(WidgetIcon::new(None, px(24.0)))
                .child(WidgetIcon::new(Some("widgets/candlestick.png"), px(24.0)))
        }
    }

    /// A widget that declares no icon is drawn with the gallery's own, so a widget that has
    /// not been given a picture yet is still a picture beside its name.
    #[gpui_kit::test]
    fn an_icon_should_be_the_declared_one_and_the_galleries_own_without_one(
        cx: &mut TestAppContext,
    ) {
        cx.update(base_ui::style::theme::install);
        let (_, cx) = cx.add_window_view(|_, _| Icons);
        cx.run_until_parked();

        assert!(cx.debug_bounds("WIDGET_GALLERY_ICON_FALLBACK").is_some());
        assert!(cx.debug_bounds("WIDGET_GALLERY_ICON_DECLARED").is_some());
    }
}

use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt as _,
};
use gpui_kit::{
    App, ElementId, InteractiveElement as _, IntoElement, ParentElement, RenderOnce, Styled as _,
    Window, WindowControlArea,
};

use super::sidebar::{SIDEBAR_ICON_SIZE, SIDEBAR_ITEM_SIZE};

/// The id each system button is drawn under, which the window's visual tests also find it by.
pub(crate) const WINDOW_MINIMIZE: &str = "WINDOW_MINIMIZE";
pub(crate) const WINDOW_MAXIMIZE: &str = "WINDOW_MAXIMIZE";
pub(crate) const WINDOW_CLOSE: &str = "WINDOW_CLOSE";

enum WindowSystemButtonKind {
    Minimize,
    Maximize,
    Close,
}

#[derive(IntoElement)]
pub struct WindowSystemButton {
    id: ElementId,
    kind: WindowSystemButtonKind,
}

impl WindowSystemButton {
    pub(crate) fn minimize() -> Self {
        Self {
            id: WINDOW_MINIMIZE.into(),
            kind: WindowSystemButtonKind::Minimize,
        }
    }

    pub(crate) fn maximize() -> Self {
        Self {
            id: WINDOW_MAXIMIZE.into(),
            kind: WindowSystemButtonKind::Maximize,
        }
    }

    pub(crate) fn close() -> Self {
        Self {
            id: WINDOW_CLOSE.into(),
            kind: WindowSystemButtonKind::Close,
        }
    }
}

impl RenderOnce for WindowSystemButton {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self { id, kind } = self;

        let (icon, area, selector) = match kind {
            WindowSystemButtonKind::Minimize => {
                (IconKind::Minus, WindowControlArea::Min, WINDOW_MINIMIZE)
            }
            WindowSystemButtonKind::Maximize => (
                if window.is_maximized() {
                    IconKind::CopySimple
                } else {
                    IconKind::Square
                },
                WindowControlArea::Max,
                WINDOW_MAXIMIZE,
            ),
            WindowSystemButtonKind::Close => (IconKind::X, WindowControlArea::Close, WINDOW_CLOSE),
        };

        // The buttons are the title bar's other controls: the sidebar toggle's square and
        // ghost paint, so all of them read as one set of controls rather than window chrome
        // drawn beside the app's own.
        Button::new(id)
            .ghost()
            .occlude()
            .h(SIDEBAR_ITEM_SIZE)
            .w(SIDEBAR_ITEM_SIZE)
            .justify_center()
            .debug_selector(move || selector.into())
            .window_control_area(area)
            .child(Icon::new(icon).size(SIDEBAR_ICON_SIZE))
    }
}

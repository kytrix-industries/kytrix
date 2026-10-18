use base_ui::{
    animation::ANIMATION_TICK,
    icon::{Icon, IconKind},
    style::{color::ResolvedColor, theme::ThemeExt},
};
use gpui_kit::{
    App, ElementId, InteractiveElement, IntoElement, MouseButton, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, Window, WindowControlArea,
    base::{Easing, Transition, transition},
    div,
    prelude::FluentBuilder,
};
use settings_ui::entry::show_settings;

#[derive(Clone)]
enum WindowSystemButtonKind {
    Minimize,
    Maximize,
    Close,
    Settings,
}

#[derive(IntoElement)]
pub struct WindowSystemButton {
    id: ElementId,
    kind: WindowSystemButtonKind,
}

impl WindowSystemButton {
    pub(crate) fn minimize() -> Self {
        Self {
            id: "window-minimize".into(),
            kind: WindowSystemButtonKind::Minimize,
        }
    }

    pub(crate) fn maximize() -> Self {
        Self {
            id: "window-maximize".into(),
            kind: WindowSystemButtonKind::Maximize,
        }
    }

    pub(crate) fn close() -> Self {
        Self {
            id: "window-close".into(),
            kind: WindowSystemButtonKind::Close,
        }
    }

    pub(crate) fn settings() -> Self {
        Self {
            id: "window-settings".into(),
            kind: WindowSystemButtonKind::Settings,
        }
    }
}

impl RenderOnce for WindowSystemButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { id, kind } = self;

        let hover_state = window.use_keyed_state((id.clone(), "hover"), cx, |_, _| false);
        let hover = *hover_state.read(cx);

        let mouse_down_state = window.use_keyed_state((id.clone(), "mouse_down"), cx, |_, _| false);
        let mouse_down = *mouse_down_state.read(cx);

        let roles = &cx.theme().roles;
        let is_window_active = window.is_window_active();
        // A window's buttons recede when it loses focus: the icon drops from body text to the
        // secondary text role rather than to a surface color, which is what made it disappear.
        // Hover and press are the brand scale's two steps, and the transition between the states
        // is resolved in Oklab by `ResolvedColor`; the paint only reads `.hsla`.
        let fg = match (mouse_down, hover) {
            (true, _) => roles.brand.active,
            (false, true) => roles.brand.hover,
            (false, false) if is_window_active => roles.fg.default,
            (false, false) => roles.fg.muted,
        };

        let fg: ResolvedColor = transition(
            (id.clone(), "fg"),
            fg,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        div()
            .id(id)
            .occlude()
            .size_6()
            .flex()
            .items_center()
            .justify_center()
            .child(
                Icon::new(match kind {
                    WindowSystemButtonKind::Minimize => IconKind::MinusBold,
                    WindowSystemButtonKind::Maximize => {
                        if window.is_maximized() {
                            IconKind::CopySimpleBold
                        } else {
                            IconKind::SquareBold
                        }
                    }
                    WindowSystemButtonKind::Close => IconKind::XBold,
                    WindowSystemButtonKind::Settings => IconKind::GearSix,
                })
                .size_3p5()
                .text_color(fg.hsla),
            )
            .when(matches!(kind, WindowSystemButtonKind::Minimize), |this| {
                this.window_control_area(WindowControlArea::Min)
            })
            .when(matches!(kind, WindowSystemButtonKind::Maximize), |this| {
                this.window_control_area(WindowControlArea::Max)
            })
            .when(matches!(kind, WindowSystemButtonKind::Close), |this| {
                this.window_control_area(WindowControlArea::Close)
            })
            .when(matches!(kind, WindowSystemButtonKind::Settings), |this| {
                this.on_click(move |_event, window, cx| show_settings(window, cx))
            })
            .on_hover(move |hovered, _, cx| hover_state.write(cx, *hovered))
            .on_mouse_down(MouseButton::Left, {
                let mouse_down_state = mouse_down_state.clone();
                move |_, _window, cx| mouse_down_state.write(cx, true)
            })
            .on_mouse_up(MouseButton::Left, {
                let mouse_down_state = mouse_down_state.clone();
                move |_, _, cx| mouse_down_state.write(cx, false)
            })
            .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                mouse_down_state.write(cx, false)
            })
    }
}

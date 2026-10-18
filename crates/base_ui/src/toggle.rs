use std::rc::Rc;

use gpui_kit::{
    AnyElement, App, Div, ElementId, InteractiveElement, Interactivity, IntoElement, ParentElement,
    RenderOnce, SharedString, Stateful, StatefulInteractiveElement, StyleRefinement, Styled,
    Window,
    base::{Disableable, Easing, Transition, transition},
    div,
    prelude::FluentBuilder,
};

use crate::{
    animation::ANIMATION_TICK,
    icon::{Icon, IconKind},
    size::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle},
    style::theme::ThemeExt,
    tooltip::{Tooltip, TooltipExt},
};

#[derive(Default, Copy, Clone)]
enum ToggleVariant {
    #[default]
    Checkbox,
    Radio,
}

#[derive(IntoElement)]
pub struct Toggle {
    id: ElementId,
    variant: ToggleVariant,
    base: Stateful<Div>,
    unified_size: UnifiedSize,
    checked: bool,
    label: SharedString,
    disabled: bool,
    on_change: Option<Rc<dyn Fn(bool, &mut Window, &mut App)>>,
    tooltip: Option<AnyElement>,
}

impl Styled for Toggle {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for Toggle {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Toggle {}

impl Disableable for Toggle {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl UnifiedSizable for Toggle {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = size;
        self
    }
}

impl TooltipExt for Toggle {
    fn with_tooltip(mut self, content: impl IntoElement) -> Self {
        self.tooltip = Some(content.into_any_element());
        self
    }
}

impl Toggle {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        let base = div().id(id.clone());

        Self {
            id,
            variant: ToggleVariant::default(),
            base,
            unified_size: UnifiedSize::Medium,
            checked: false,
            label: Default::default(),
            disabled: false,
            on_change: None,
            tooltip: None,
        }
    }

    pub fn checkbox(mut self) -> Self {
        self.variant = ToggleVariant::Checkbox;
        self
    }

    pub fn radio(mut self) -> Self {
        self.variant = ToggleVariant::Radio;
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }

    pub fn on_change(mut self, on_change: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(on_change));
        self
    }
}

impl RenderOnce for Toggle {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Toggle {
            id,
            variant,
            base,
            unified_size,
            checked,
            label,
            disabled,
            on_change,
            tooltip,
        } = self;

        let theme = cx.theme();

        let state_colors = if disabled {
            theme.components.toggle.disabled
        } else {
            theme.components.toggle.enabled
        };

        let geometry_color = transition(
            (id.clone(), "geometry_color"),
            if checked {
                state_colors.checked
            } else {
                state_colors.unchecked
            },
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let geometry_opacity = transition(
            (id.clone(), "geometry_opacity"),
            if checked { 1.0 } else { 0.0 },
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let base = base
            .flex()
            .items_center()
            .text_color(state_colors.foreground.hsla)
            .unified_sized(unified_size)
            .child(
                div()
                    .size(unified_size.icon_size() * 1.8)
                    .relative()
                    .child(
                        Icon::new(match variant {
                            ToggleVariant::Checkbox => IconKind::CheckSquare,
                            ToggleVariant::Radio => IconKind::RadioButton,
                        })
                        .absolute()
                        .inset_0()
                        .size_full()
                        .text_color(geometry_color.hsla)
                        .opacity(geometry_opacity),
                    )
                    .child(
                        Icon::new(match variant {
                            ToggleVariant::Checkbox => IconKind::Square,
                            ToggleVariant::Radio => IconKind::Circle,
                        })
                        .absolute()
                        .inset_0()
                        .size_full()
                        .text_color(geometry_color.hsla)
                        .opacity(1.0 - geometry_opacity),
                    ),
            )
            .child(label)
            .when_else(
                disabled,
                |this| this.cursor_not_allowed(),
                |this| {
                    this.when_some(on_change, |this, on_change| {
                        this.on_click(move |_, window, cx| on_change(!checked, window, cx))
                    })
                },
            );

        if let Some(tooltip) = tooltip {
            Tooltip::new((id, "tooltip"), base)
                .content(tooltip)
                .into_any_element()
        } else {
            base.into_any_element()
        }
    }
}

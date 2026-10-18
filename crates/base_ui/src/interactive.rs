use gpui_kit::{
    AnyElement, App, Div, ElementId, InteractiveElement, Interactivity, IntoElement, MouseButton,
    ParentElement, RenderOnce, Stateful, StatefulInteractiveElement, StyleRefinement, Styled,
    Window,
    base::{Disableable, Easing, Selectable, StyledExt, Transition, transition},
    div,
    prelude::FluentBuilder,
};
use smallvec::SmallVec;

use crate::{
    animation::ANIMATION_TICK,
    size::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle},
    style::theme::ThemeExt,
    tooltip::{Tooltip, TooltipExt},
};

#[derive(Default, Copy, Clone, Eq, PartialEq)]
pub(crate) enum InteractiveVariant {
    #[default]
    Filled,
    Tonal,
    Outline,
    Ghost,
}

pub trait InteractiveVariantExt {
    fn filled(self) -> Self;
    fn tonal(self) -> Self;
    fn outline(self) -> Self;
    fn ghost(self) -> Self;
}

#[derive(Default, Copy, Clone, Eq, PartialEq)]
pub(crate) enum InteractiveState {
    #[default]
    Normal,
    Hover,
    Active,
    Disabled,
}

#[derive(IntoElement)]
pub struct Interactive {
    id: ElementId,
    base: Stateful<Div>,
    unified_size: Option<UnifiedSize>,
    variant: InteractiveVariant,
    children: SmallVec<[AnyElement; 4]>,
    style: StyleRefinement,
    disabled: bool,
    selected: bool,
    tooltip: Option<AnyElement>,
}

impl Interactive {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        let base = div().id(id.clone());

        Self {
            id,
            base,
            unified_size: None,
            variant: InteractiveVariant::default(),
            children: SmallVec::new(),
            style: StyleRefinement::default(),
            disabled: false,
            selected: false,
            tooltip: None,
        }
    }
}

impl InteractiveVariantExt for Interactive {
    fn filled(mut self) -> Self {
        self.variant = InteractiveVariant::Filled;
        self
    }

    fn tonal(mut self) -> Self {
        self.variant = InteractiveVariant::Tonal;
        self
    }

    fn outline(mut self) -> Self {
        self.variant = InteractiveVariant::Outline;
        self
    }

    fn ghost(mut self) -> Self {
        self.variant = InteractiveVariant::Ghost;
        self
    }
}

impl UnifiedSizable for Interactive {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = Some(size);
        self
    }
}

impl InteractiveElement for Interactive {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Interactive {}

impl ParentElement for Interactive {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl Styled for Interactive {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Selectable for Interactive {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl Disableable for Interactive {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl TooltipExt for Interactive {
    fn with_tooltip(mut self, content: impl IntoElement) -> Self {
        self.tooltip = Some(content.into_any_element());
        self
    }
}

impl RenderOnce for Interactive {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            base,
            unified_size,
            variant,
            children,
            style,
            disabled,
            selected,
            tooltip,
        } = self;

        let hover_state = window.use_keyed_state((id.clone(), "hover"), cx, |_, _| false);
        let hovered = *hover_state.read(cx);

        let active_state = window.use_keyed_state((id.clone(), "active"), cx, |_, _| false);
        let active = *active_state.read(cx) || selected;

        let theme = cx.theme();

        let variant_colors = match variant {
            InteractiveVariant::Filled => &theme.components.interactive.filled,
            InteractiveVariant::Tonal => &theme.components.interactive.tonal,
            InteractiveVariant::Outline => &theme.components.interactive.outline,
            InteractiveVariant::Ghost => &theme.components.interactive.ghost,
        };

        let state_colors = match (disabled, active, hovered) {
            (true, _, _) => variant_colors.disabled,
            (false, true, _) => variant_colors.active,
            (false, false, true) => variant_colors.hover,
            (false, false, false) => variant_colors.normal,
        };

        let bg = transition(
            (id.clone(), "bg"),
            state_colors.background,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let fg = transition(
            (id.clone(), "fg"),
            state_colors.foreground,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let border = transition(
            (id.clone(), "border"),
            state_colors.border,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let base = base
            .bg(bg.hsla)
            .text_color(fg.hsla)
            .border_1()
            .border_color(border.hsla)
            .flex()
            .items_center()
            .children(children)
            .when_some(unified_size, |this, unified_size| {
                this.unified_sized(unified_size)
            })
            .when(disabled, |this| this.cursor_not_allowed())
            .when(!disabled, |this| {
                this.on_mouse_down(MouseButton::Left, {
                    let active_state = active_state.clone();
                    move |_, _, cx| {
                        active_state.write(cx, true);
                    }
                })
                .on_mouse_up(MouseButton::Left, {
                    let active_state = active_state.clone();
                    move |_, _, cx| {
                        active_state.write(cx, false);
                    }
                })
                .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                    active_state.write(cx, false);
                })
            })
            .on_hover(move |hovered, _, cx| hover_state.write(cx, *hovered))
            .refine_style(&style);

        if let Some(tooltip) = tooltip {
            Tooltip::new((id, "tooltip"), base)
                .content(tooltip)
                .into_any_element()
        } else {
            base.into_any_element()
        }
    }
}

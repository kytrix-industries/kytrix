use gpui_kit::{
    AnyElement, App, ElementId, InteractiveElement, Interactivity, IntoElement, ParentElement,
    RenderOnce, Role, StatefulInteractiveElement, StyleRefinement, Styled, Window,
    base::{Disableable, Selectable, StyledExt},
};

use crate::{
    interactive::{Interactive, InteractiveVariantExt},
    size::{UnifiedSizable, UnifiedSize},
    tooltip::TooltipExt,
};

#[derive(IntoElement)]
pub struct Button {
    base: Interactive,
    style: StyleRefinement,
}

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            base: Interactive::new(id.into()).medium(),
            style: StyleRefinement::default(),
        }
    }

    /// Keeps the button in its hover paint while a popup it opened is up.
    ///
    /// The popup's sheet covers the button, so its own hover listener reports the pointer
    /// leaving. This asks the button to paint the hover state anyway; see
    /// [`Interactive::force_hover`].
    pub fn force_hover(mut self, force_hover: bool) -> Self {
        self.base = self.base.force_hover(force_hover);
        self
    }
}

impl InteractiveVariantExt for Button {
    fn filled(mut self) -> Self {
        self.base = self.base.filled();
        self
    }

    fn tonal(mut self) -> Self {
        self.base = self.base.tonal();
        self
    }

    fn outline(mut self) -> Self {
        self.base = self.base.outline();
        self
    }

    fn ghost(mut self) -> Self {
        self.base = self.base.ghost();
        self
    }
}

impl Styled for Button {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl InteractiveElement for Button {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Button {}

impl ParentElement for Button {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.base.extend(elements)
    }
}

impl Selectable for Button {
    fn selected(mut self, selected: bool) -> Self {
        self.base = self.base.selected(selected);
        self
    }

    fn is_selected(&self) -> bool {
        self.base.is_selected()
    }
}

impl Disableable for Button {
    fn disabled(mut self, disabled: bool) -> Self {
        self.base = self.base.disabled(disabled);
        self
    }
}

impl UnifiedSizable for Button {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.base = self.base.with_unified_size(size);
        self
    }
}

impl TooltipExt for Button {
    fn with_tooltip(mut self, content: impl IntoElement) -> Self {
        self.base = self.base.with_tooltip(content);
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Button { base, style } = self;

        base.rounded_sm().role(Role::Button).refine_style(&style)
    }
}

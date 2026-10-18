use std::collections::VecDeque;

use gpui_kit::{
    AnyElement, App, Empty, IntoElement, ParentElement, RenderOnce, StyleRefinement, Styled,
    Window, base::StyledExt, div, prelude::FluentBuilder,
};

use crate::{button::Button, select::Select};

enum JoinElement {
    Button(Button),
    Select(Select),
}

impl JoinElement {
    fn into_join_child(self, is_first: bool, is_last: bool) -> AnyElement {
        match self {
            JoinElement::Button(button) => {
                square_shared_corners(button, is_first, is_last).into_any_element()
            }
            JoinElement::Select(select) => {
                square_shared_corners(select, is_first, is_last).into_any_element()
            }
        }
    }
}

#[derive(Default, IntoElement)]
pub struct Join {
    elements: VecDeque<JoinElement>,
    style: StyleRefinement,
}

impl Join {
    pub fn button(mut self, button: Button) -> Self {
        self.elements.push_back(JoinElement::Button(button));
        self
    }

    pub fn select(mut self, select: Select) -> Self {
        self.elements.push_back(JoinElement::Select(select));
        self
    }
}

impl Styled for Join {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

fn square_shared_corners<T: Styled + IntoElement>(element: T, is_first: bool, is_last: bool) -> T {
    element
        .when(!is_first, |element| element.rounded_l_none().border_l_0())
        .when(!is_last, |element| element.rounded_r_none())
}

impl RenderOnce for Join {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self {
            mut elements,
            style,
        } = self;

        let Some(first) = elements.pop_front() else {
            return Empty.into_any_element();
        };

        if elements.is_empty() {
            return first.into_join_child(true, true);
        }

        let mut base = div()
            .flex()
            .flex_row()
            .child(first.into_join_child(true, false));

        while let Some(element) = elements.pop_front() {
            let is_last = elements.is_empty();
            base = base.child(element.into_join_child(false, is_last));
        }

        base.refine_style(&style).into_any_element()
    }
}

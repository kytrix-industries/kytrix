use gpui_kit::{Context, IntoElement, ParentElement, Render, Styled, Window, div};

#[derive(Default)]
pub struct Quant;

impl Render for Quant {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child("Coming Soon")
    }
}

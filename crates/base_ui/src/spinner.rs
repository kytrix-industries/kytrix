use gpui_kit::*;

use crate::{animation::ANIMATION_TICK, icon::IconKind};

#[derive(IntoElement)]
pub struct Spinner {
    id: ElementId,
}

impl Spinner {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self { id: id.into() }
    }
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Spinner { id } = self;

        // A spinner states no colour of its own: it is a mark inside something that already has
        // one, so it takes the text colour around it — white on a destructive button, muted on a
        // placeholder. Pinning it to a token of its own would make it wrong on every fill that is
        // not the page.
        svg()
            .path(IconKind::CircleNotch.path())
            .text_color(window.text_style().color)
            .size_full()
            .with_animation(
                (id, "animation"),
                Animation::new(ANIMATION_TICK * 6).repeat(),
                |this, delta| this.with_transformation(Transformation::rotate(percentage(delta))),
            )
    }
}

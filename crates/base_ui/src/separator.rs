use gpui_kit::{
    App, IntoElement, RenderOnce, StyleRefinement, Styled, Window, base::StyledExt, div,
    prelude::FluentBuilder,
};

use crate::style::theme::ThemeExt;

#[derive(IntoElement)]
pub struct Separator {
    is_horizontal: bool,
    style: StyleRefinement,
}

impl Separator {
    pub fn horizontal() -> Self {
        Self {
            is_horizontal: true,
            style: StyleRefinement::default(),
        }
    }

    pub fn vertical() -> Self {
        Self {
            is_horizontal: false,
            style: StyleRefinement::default(),
        }
    }
}

impl Styled for Separator {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Separator {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            is_horizontal,
            style,
        } = self;

        div()
            .flex_shrink_0()
            .bg(cx.theme().roles.line.divider.hsla)
            .when_else(
                is_horizontal,
                |this| this.h_px().w_full(),
                |this| this.w_px().self_stretch(),
            )
            .refine_style(&style)
    }
}

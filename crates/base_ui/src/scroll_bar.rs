use gpui_kit::{
    base::{ScrollbarHandle, ScrollbarMode},
    *,
};

use crate::style::{color::ResolvedColor, theme::ThemeExt};

#[derive(IntoElement)]
pub struct Scrollbar<H>
where
    H: ScrollbarHandle + Clone,
{
    is_horizontal: bool,
    handler: H,
    mode: ScrollbarMode,
}

impl<H> Scrollbar<H>
where
    H: ScrollbarHandle + Clone,
{
    pub fn horizontal(handler: H) -> Self {
        Self {
            is_horizontal: true,
            handler,
            mode: ScrollbarMode::default(),
        }
    }

    pub fn vertical(handler: H) -> Self {
        Self {
            is_horizontal: false,
            handler,
            mode: ScrollbarMode::default(),
        }
    }

    pub fn mode(mut self, mode: ScrollbarMode) -> Self {
        self.mode = mode;
        self
    }
}

impl<H> RenderOnce for Scrollbar<H>
where
    H: ScrollbarHandle + Clone,
{
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Scrollbar {
            is_horizontal,
            handler,
            mode,
        } = self;

        let theme = cx.theme();
        // The track is the scrollbar's, not the theme's: it is never painted, so it is the
        // transparent role rather than a raw color of its own.
        let clear = ResolvedColor::transparent().hsla;

        let base = if is_horizontal {
            gpui_kit::base::Scrollbar::horizontal(&handler)
        } else {
            gpui_kit::base::Scrollbar::vertical(&handler)
        };

        base.mode(mode).styles(|s| {
            s.thumb(|s| {
                s.bg(theme.components.scrollbar.normal.hsla)
                    .width(4.)
                    .inset(0.)
                    .radius(1.)
            })
            .thumb_hover(|s| {
                s.bg(theme.components.scrollbar.active.hsla)
                    .width(4.)
                    .inset(0.)
                    .radius(1.)
            })
            .thumb_active(|s| {
                s.bg(theme.components.scrollbar.active.hsla)
                    .width(4.)
                    .inset(0.)
                    .radius(1.)
            })
            .track(|s| s.bg(clear))
            .track_hover(|s| s.bg(clear))
            .track_active(|s| s.bg(clear))
        })
    }
}

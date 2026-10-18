mod support;

use base_ui::{assets::Assets, scroll_bar::Scrollbar, style::theme::ThemeExt};
use gpui_kit::{
    AppContext, Bounds, Context, InteractiveElement, IntoElement, ParentElement, Render,
    ScrollHandle, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, div, px,
    size,
};

struct ScrollBarExample {
    vertical: ScrollHandle,
    horizontal: ScrollHandle,
}

impl ScrollBarExample {
    fn new() -> Self {
        Self {
            vertical: ScrollHandle::new(),
            horizontal: ScrollHandle::new(),
        }
    }

    fn vertical_area(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .relative()
            .overflow_hidden()
            .w(px(360.))
            .h(px(200.))
            .rounded_sm()
            .child(
                div()
                    .id("scroll-vertical")
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&self.vertical)
                    .bg(theme.roles.surface.canvas.hsla)
                    .children((0..40).map(|index| {
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .child(format!("Row {index}"))
                    })),
            )
            .child(Scrollbar::vertical(self.vertical.clone()))
    }

    fn horizontal_area(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .relative()
            .overflow_hidden()
            .w(px(360.))
            .h(px(96.))
            .rounded_sm()
            .child(
                div()
                    .id("scroll-horizontal")
                    .size_full()
                    .flex()
                    .flex_row()
                    .overflow_x_scroll()
                    .track_scroll(&self.horizontal)
                    .bg(theme.roles.surface.canvas.hsla)
                    .children((0..40).map(|index| {
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .child(format!("Column {index}"))
                    })),
            )
            .child(Scrollbar::horizontal(self.horizontal.clone()))
    }
}

impl Render for ScrollBarExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_6()
            .child(self.vertical_area(cx))
            .child(self.horizontal_area(cx))
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(560.), px(480.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| ScrollBarExample::new()),
        )
        .expect("failed to open window");
    });
}

mod support;

use base_ui::{assets::Assets, spinner::Spinner, style::theme::ThemeExt};
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window, WindowBounds,
    WindowOptions, div, px, size,
};

struct SpinnerExample;

impl Render for SpinnerExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .bg(cx.theme().roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_6()
                    .child(spinner("spinner-3", 12.))
                    .child(spinner("spinner-4", 16.))
                    .child(spinner("spinner-6", 24.))
                    .child(spinner("spinner-8", 32.))
                    .child(spinner("spinner-12", 48.)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(spinner("spinner-inline", 16.))
                    .child("Loading…"),
            )
    }
}

fn spinner(id: &'static str, side: f32) -> impl IntoElement {
    div().size(px(side)).child(Spinner::new(id))
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(520.), px(320.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| SpinnerExample),
        )
        .expect("failed to open window");
    });
}

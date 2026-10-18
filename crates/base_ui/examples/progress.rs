mod support;

use base_ui::{assets::Assets, progress::Progress, style::theme::ThemeExt};
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window, WindowBounds,
    WindowOptions, div, px, size,
};

struct ProgressExample;

impl Render for ProgressExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .bg(cx.theme().roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            // The bar has no colour of its own to choose: every one of these is the same
            // `surface.control` track under the same `brand` fill, and only the value differs. The
            // empty one still shows the track, and the overflowing one is clamped to it.
            .child(bar(Progress::new("progress-empty").value(0.)))
            .child(bar(Progress::new("progress-low").value(15.)))
            .child(bar(Progress::new("progress-half").value(50.)))
            .child(bar(Progress::new("progress-most").value(90.)))
            .child(bar(Progress::new("progress-full").value(100.)))
            .child(bar(Progress::new("progress-overflow").value(150.)))
    }
}

fn bar(progress: Progress) -> impl IntoElement {
    div().w(px(360.)).child(progress)
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(520.), px(520.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| ProgressExample),
        )
        .expect("failed to open window");
    });
}

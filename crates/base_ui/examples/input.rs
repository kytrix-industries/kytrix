mod support;

use base_ui::{
    assets::Assets,
    icon::IconKind,
    input::{Input, InputStateBuilder},
    size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window, WindowBounds,
    WindowOptions, div, px, size,
};

struct InputExample;

impl Render for InputExample {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let basic = window.use_keyed_state("input-basic", cx, |window, cx| {
            InputStateBuilder::new("input-basic-state")
                .placeholder("Basic")
                .build(window, cx)
        });

        let currency = window.use_keyed_state("input-currency", cx, |window, cx| {
            InputStateBuilder::new("input-currency-state")
                .placeholder("Currency")
                .currency(2)
                .build(window, cx)
        });

        let prefixed = window.use_keyed_state("input-prefixed", cx, |window, cx| {
            InputStateBuilder::new("input-prefixed-state")
                .placeholder("With prefix")
                .build(window, cx)
        });

        let password = window.use_keyed_state("input-password", cx, |window, cx| {
            InputStateBuilder::new("input-password-state")
                .placeholder("Password")
                .password()
                .build(window, cx)
        });

        let disabled = window.use_keyed_state("input-disabled", cx, |window, cx| {
            let mut state = InputStateBuilder::new("input-disabled-state")
                .placeholder("Disabled")
                .build(window, cx);
            state.update_disabled(true, cx);
            state
        });

        let theme = cx.theme();

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&basic).small().clearable(true)),
                    )
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&currency).medium().clearable(true)),
                    )
                    .child(
                        div().w(px(220.)).child(
                            Input::new(&prefixed)
                                .large()
                                .prefix_icon(IconKind::CircleNotch)
                                .clearable(true),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&password).large().clearable(true)),
                    )
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&disabled).large().clearable(true)),
                    ),
            )
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(880.), px(360.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| InputExample),
        )
        .expect("failed to open window");
    });
}

mod support;

use base_ui::{
    assets::Assets,
    icon::{Icon, IconKind},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, Hsla, IntoElement, ParentElement, Render, Styled, Transformation,
    Window, WindowBounds, WindowOptions, div, px, radians, size,
};

struct IconExample;

impl Render for IconExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = theme.roles.fg.default.hsla;

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_center()
                    .gap_6()
                    .children(
                        [
                            (IconKind::X, "X"),
                            (IconKind::Circle, "Circle"),
                            (IconKind::RadioButton, "RadioButton"),
                            (IconKind::Square, "Square"),
                            (IconKind::CheckSquare, "CheckSquare"),
                            (IconKind::CircleNotch, "CircleNotch"),
                            (IconKind::CaretDown, "CaretDown"),
                            (IconKind::Eye, "Eye"),
                            (IconKind::EyeClosed, "EyeClosed"),
                        ]
                        .into_iter()
                        .map(|(kind, name)| {
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(Icon::new(kind).size_8().text_color(content))
                                .child(div().text_xs().child(name))
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        Icon::new(IconKind::CircleNotch)
                            .size_4()
                            .text_color(content),
                    )
                    .child(
                        Icon::new(IconKind::CircleNotch)
                            .size_6()
                            .text_color(content),
                    )
                    .child(
                        Icon::new(IconKind::CircleNotch)
                            .size_8()
                            .text_color(content),
                    )
                    .child(
                        Icon::new(IconKind::CircleNotch)
                            .size_12()
                            .text_color(content),
                    ),
            )
            .child(
                div().flex().flex_row().items_center().gap_4().children(
                    [
                        theme.roles.brand.base.hsla,
                        theme.roles.surface.raised.hsla,
                        theme.roles.market.up.hsla,
                        theme.roles.market.down.hsla,
                        theme.roles.status.danger.base.hsla,
                        theme.components.chart.series[0].hsla,
                        theme.components.chart.series[1].hsla,
                        theme.components.chart.series[2].hsla,
                        theme.roles.fg.default.hsla,
                    ]
                    .into_iter()
                    .map(|color: Hsla| Icon::new(IconKind::Square).size_6().text_color(color)),
                ),
            )
            .child(div().flex().flex_row().items_center().gap_4().children(
                [0., 90., 180., 270.].into_iter().map(|degrees| {
                    Icon::new(IconKind::CaretDown)
                        .size_6()
                        .text_color(content)
                        .transformation(Transformation::rotate(radians(degrees)))
                }),
            ))
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(760.), px(420.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| IconExample),
        )
        .expect("failed to open window");
    });
}

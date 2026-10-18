mod support;

use base_ui::{
    assets::Assets, button::Button, interactive::InteractiveVariantExt, tooltip::Tooltip,
};
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window, WindowBounds,
    WindowOptions, base::Placement, div, px, size,
};

struct TooltipExample;

impl Render for TooltipExample {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_between()
            .child({
                let trigger = Button::new("trigger1").tonal().child("Hover top");
                Tooltip::new("hover1", trigger).content("This is a tooltip")
            })
            .child({
                let trigger = Button::new("trigger2").tonal().child("Hover top");
                Tooltip::new("hover2", trigger)
                    .placement(Placement::Top)
                    .content("This is a tooltip")
            })
            .child({
                let trigger = Button::new("trigger3")
                    .tonal()
                    .child("Hover preferred bottom");
                Tooltip::new("hover3", trigger).content("This is a tooltip")
            })
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(960.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| TooltipExample),
        )
        .expect("failed to open window");
    });
}

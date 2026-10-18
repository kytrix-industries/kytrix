mod support;

use base_ui::{
    assets::Assets, button::Button, interactive::InteractiveVariantExt, size::UnifiedSizable,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, base::Disableable,
    div, px, size,
};

struct ButtonExample {
    click_count: Entity<usize>,
}

impl ButtonExample {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            click_count: cx.new(|_| 0),
        }
    }
}

impl Render for ButtonExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let click_count = *self.click_count.read(cx);
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
            .child(variant_row())
            .child(size_row())
            .child(disabled_row())
            // No variant call at all: a button asks for nothing and gets `default`, which is
            // the one thing about this component that is not written down at the call site.
            .child(
                Button::new("counter")
                    .large()
                    .child(format!("Clicked {click_count} times"))
                    .on_click({
                        let click_count = self.click_count.clone();
                        move |_event, _window, cx| {
                            click_count.update(cx, |count, cx| {
                                *count += 1;
                                cx.notify();
                            });
                        }
                    }),
            )
    }
}

/// One button per variant, which is the whole of what a button can be.
///
/// There is no second axis to show: shadcn names the colour as part of the variant, so
/// `destructive` is a red button rather than a red version of something else.
fn variant_row() -> impl IntoElement {
    row([
        Button::new("variant-filled").child("Filled"),
        Button::new("variant-tonal").tonal().child("Tonal"),
        Button::new("variant-outline").outline().child("Outline"),
        Button::new("variant-ghost").ghost().child("Ghost"),
    ])
}

/// The three sizes, every one of them on the default variant.
fn size_row() -> impl IntoElement {
    row([
        Button::new("size-small").small().child("Small"),
        Button::new("size-medium").medium().child("Medium"),
        Button::new("size-large").large().child("Large"),
    ])
}

/// Every variant disabled.
///
/// The two that paint no fill of their own fade their label rather than taking a muted pill, so
/// a disabled ghost still looks like a ghost.
fn disabled_row() -> impl IntoElement {
    row([
        Button::new("disabled-filled")
            .disabled(true)
            .child("Filled"),
        Button::new("disabled-tonal")
            .tonal()
            .disabled(true)
            .child("Tonal"),
        Button::new("disabled-outline")
            .outline()
            .disabled(true)
            .child("Outline"),
        Button::new("disabled-ghost")
            .ghost()
            .disabled(true)
            .child("Ghost"),
    ])
}

fn row(buttons: impl IntoIterator<Item = Button>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .justify_center()
        .gap_2()
        .children(buttons)
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(900.), px(460.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(ButtonExample::new),
        )
        .expect("failed to open window");
    });
}

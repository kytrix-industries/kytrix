mod support;

use base_ui::{
    assets::Assets,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, Hsla, IntoElement, ParentElement, Render, Rgba, Styled,
    Subscription, Window, WindowBounds, WindowOptions, div, hsla, px, size,
};

/// The accent the picker starts on.
const DEFAULT_ACCENT: Hsla = hsla(0.55, 0.8, 0.53, 1.0);

struct ColorPickerExample {
    accent: Hsla,
    state: Option<Entity<ColorPickerState>>,
    subscription: Option<Subscription>,
}

impl ColorPickerExample {
    fn new() -> Self {
        Self {
            accent: DEFAULT_ACCENT,
            state: None,
            subscription: None,
        }
    }
}

impl Render for ColorPickerExample {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The picker's state needs a window, so it is created on the first render and reused by
        // id afterwards; the default only applies to the frame that creates it. The subscription
        // is stored so the accent survives as long as the window does.
        let state = ColorPickerState::use_keyed("color-picker-example", self.accent, window, cx);
        if self.state.as_ref() != Some(&state) || self.subscription.is_none() {
            self.state = Some(state.clone());
            self.subscription = Some(cx.subscribe(
                &state,
                |this, _, event: &ColorPickerEvent, cx| {
                    let ColorPickerEvent::ColorChanged(color) = event;
                    this.accent = *color;
                    cx.notify();
                },
            ));
        }

        let theme = cx.theme();
        let accent = self.accent;
        let [r, g, b, a] = rgba8(accent);
        let hex = if a < 255 {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}")
        };

        div()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(ColorPicker::new(&state))
            .child(
                div()
                    .w(px(160.))
                    .h(px(160.))
                    .rounded_md()
                    .bg(accent)
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(hex),
            )
    }
}

/// Quantizes a color to the eight-bit sRGB channels the example's hex text shows.
fn rgba8(color: Hsla) -> [u8; 4] {
    let rgba = Rgba::from(color);
    let quantize = |value: f32| (value.clamp(0., 1.) * 255.0).round() as u8;

    [
        quantize(rgba.r),
        quantize(rgba.g),
        quantize(rgba.b),
        quantize(rgba.a),
    ]
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(720.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| ColorPickerExample::new()),
        )
        .expect("failed to open window");
    });
}

mod support;

use base_ui::{
    assets::Assets,
    color_picker::{ColorPicker, ColorPickerEvent},
    size::UnifiedSize,
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
    picker: Option<Entity<ColorPicker>>,
    subscription: Option<Subscription>,
}

impl ColorPickerExample {
    fn new() -> Self {
        Self {
            accent: DEFAULT_ACCENT,
            picker: None,
            subscription: None,
        }
    }
}
impl Render for ColorPickerExample {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The picker needs a window, so it is created on the first render and stored: every later
        // render draws the same entity, keeping its color, its tab and the caret in its field.
        // The subscription is installed with it and kept for the same reason.
        if self.picker.is_none() || self.subscription.is_none() {
            let accent = self.accent;
            let picker = cx.new(|cx| ColorPicker::new(accent, UnifiedSize::Small, window, cx));
            self.subscription = Some(cx.subscribe(
                &picker,
                |this, _, event: &ColorPickerEvent, cx| {
                    // The picker reports a color once, when the panel's confirm button is pressed:
                    // a swatch, a track or a hex edit is only the panel's own preview until then.
                    let ColorPickerEvent::ColorChanged(color) = event;
                    this.accent = *color;
                    cx.notify();
                },
            ));
            self.picker = Some(picker);
        }

        let theme = cx.theme();
        let accent = self.accent;
        let [r, g, b, a] = rgba8(accent);
        let hex = if a < 255 {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}")
        };
        let picker = self
            .picker
            .as_ref()
            .expect("the picker is created above")
            .clone();

        div()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(picker)
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

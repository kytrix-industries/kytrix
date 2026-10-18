mod support;

use base_ui::{
    assets::Assets,
    button::Button,
    interactive::InteractiveVariantExt,
    layer::{Layer, LayerExt},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, DismissEvent, EventEmitter, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, div, px, size,
};

/// The dialog's content, and the dialog's state with it.
///
/// The content is a view entity of its own, which is what the dialog hosts: the dialog holds a
/// strong reference to it for as long as it is on the layer, and drops it — state and all — once it
/// has left, so nothing outside the content has to know what the content owned.
struct CustomDialog {
    /// How many times this dialog was confirmed. It belongs to the dialog and to nothing else,
    /// which is the whole point: the state lives here rather than in the view that opened it.
    confirmed: usize,
}

impl CustomDialog {
    fn new() -> Self {
        Self { confirmed: 0 }
    }
}

/// How a content asks its dialog to close. The dialog subscribes to this event when it mounts, so
/// the content needs no window to say it and does not have to be the dialog on top.
impl EventEmitter<DismissEvent> for CustomDialog {}

impl Render for CustomDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .w_96()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_lg().child("Custom Dialog"))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.roles.fg.default.hsla)
                    .child("Dialog only draws the backdrop, the panel appearance and the enter/exit transition; its content is any element."),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("dialog-cancel")
                            .child("Cancel")
                            .outline()
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("dialog-confirm")
                            .child("Confirm")
                            .filled()
                            .on_click(cx.listener(|dialog, _, _, cx| {
                                dialog.confirmed += 1;
                                println!("confirmed: {}", dialog.confirmed);

                                cx.emit(DismissEvent);
                            })),
                    ),
            )
    }
}

struct DialogExample;

impl Render for DialogExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        let open_dialog = Button::new("open-dialog")
            .tonal()
            .child("Open Dialog")
            .on_click(|_, window, cx| {
                // Made first and handed over on its own line: `cx` is what both calls need, and
                // `window.dialog(cx, cx.new(..))` would borrow it mutably twice in one call.
                let dialog = cx.new(|_| CustomDialog::new());

                window.dialog(cx, dialog);
            });

        let open_alert = Button::new("open-alert")
            .child("Open Alert")
            .outline()
            .on_click(|_, window, cx| {
                window.alert(cx, |alert, _, _| {
                    alert
                        .ok_cancel("Nice", "Do you want Nice?")
                        .cancel_text("Cancel")
                        .ok_action(|_, _, _| println!("OK"))
                        .cancel_action(|_, _| println!("Cancel"))
                });
            });

        div()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .child(open_dialog)
            .child(open_alert)
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
            |_window, cx| {
                let view = cx.new(|_| DialogExample);
                cx.new(|_| Layer::new(view))
            },
        )
        .expect("failed to open window");
    });
}

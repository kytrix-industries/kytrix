mod support;

use base_ui::{
    assets::Assets,
    button::Button,
    input::InputStateBuilder,
    interactive::InteractiveVariantExt,
    layer::{Layer, LayerExt},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, div, px, size,
};

/// What the last alert handed back, drawn under the buttons so every callback is visible.
#[derive(Default)]
struct AlertExample {
    result: SharedString,
}

impl AlertExample {
    fn report(&mut self, result: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.result = result.into();
        cx.notify();
    }
}

impl Render for AlertExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        // 1. title + message + ok
        let open_ok = Button::new("alert-ok")
            .tonal()
            .child("Ok")
            .on_click(cx.listener(|_, _, window, cx| {
                let entity = cx.entity();

                window.alert(cx, move |alert, _, _| {
                    alert
                        .ok("Delete file", "This file will be removed permanently.")
                        .ok_text("Delete")
                        .ok_action(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.report("ok: Delete file", cx));
                        })
                });
            }));

        // 2. title + message + ok + cancel
        let open_ok_cancel = Button::new("alert-ok-cancel")
            .child("Ok + Cancel")
            .outline()
            .on_click(cx.listener(|_, _, window, cx| {
                let entity = cx.entity();

                window.alert(cx, move |alert, _, _| {
                    alert
                        .ok_cancel("Discard changes", "Your unsaved changes will be lost.")
                        .ok_text("Discard")
                        .cancel_text("Keep editing")
                        .ok_action({
                            let entity = entity.clone();

                            move |_, _, cx| {
                                entity
                                    .update(cx, |this, cx| this.report("ok: Discard changes", cx));
                            }
                        })
                        .cancel_action(move |_, cx| {
                            entity
                                .update(cx, |this, cx| this.report("cancel: Discard changes", cx));
                        })
                });
            }));

        // 3. title + message + input + ok, with the value handed back to the ok action
        let open_input_ok = Button::new("alert-input-ok")
            .child("Input + Ok")
            .outline()
            .on_click(cx.listener(|_, _, window, cx| {
                let entity = cx.entity();

                window.alert(cx, move |alert, window, cx| {
                    let input = cx.new(|cx| {
                        let mut state = InputStateBuilder::new("alert-input")
                            .placeholder("File name")
                            .build(window, cx);

                        state.update_value("report.txt", window, cx);

                        state
                    });

                    alert
                        .input_ok("Rename", "Enter the new name of this file.", input)
                        .ok_text("Rename")
                        .ok_action(move |value, _, cx| {
                            let value = value.unwrap_or_default();

                            entity.update(cx, |this, cx| {
                                this.report(format!("ok: Rename to {value}"), cx);
                            });
                        })
                });
            }));

        // 4. title + message + input + ok + cancel, with a value the ok button has to accept
        let open_input_ok_cancel = Button::new("alert-input-ok-cancel")
            .child("Input + Ok + Cancel")
            .outline()
            .on_click(cx.listener(|_, _, window, cx| {
                let entity = cx.entity();

                window.alert(cx, move |alert, window, cx| {
                    let input = cx.new(|cx| {
                        InputStateBuilder::new("alert-input")
                            .placeholder("Folder name")
                            .build(window, cx)
                    });

                    alert
                        .input_ok_cancel("New folder", "The name may not be empty.", input)
                        .ok_text("Create")
                        .cancel_text("Cancel")
                        .validate(|value| {
                            if value.is_empty() {
                                Err(SharedString::new_static("A name is required."))
                            } else {
                                Ok(())
                            }
                        })
                        .ok_action({
                            let entity = entity.clone();

                            move |value, _, cx| {
                                let value = value.unwrap_or_default();

                                entity.update(cx, |this, cx| {
                                    this.report(format!("ok: New folder {value}"), cx);
                                });
                            }
                        })
                        .cancel_action(move |_, cx| {
                            entity.update(cx, |this, cx| this.report("cancel: New folder", cx));
                        })
                });
            }));

        div()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(open_ok)
                    .child(open_ok_cancel)
                    .child(open_input_ok)
                    .child(open_input_ok_cancel),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.roles.fg.default.hsla)
                    .child(self.result.clone()),
            )
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
                let view = cx.new(|_| AlertExample::default());
                cx.new(|_| Layer::new(view))
            },
        )
        .expect("failed to open window");
    });
}

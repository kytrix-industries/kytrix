use std::{
    rc::Rc,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    base::StyledExt, div, prelude::FluentBuilder,
};

use crate::{
    button::Button,
    input::{Input, InputState},
    interactive::InteractiveVariantExt,
    join::Join,
    layer::LayerExt,
    markdown::view::{Markdown, MarkdownState},
    style::theme::ThemeExt,
};

/// Checks the value of the input field when the ok button is clicked. `Err` carries the message
/// shown under the field, which also blocks the dismissal.
type ValidatorFn = Rc<dyn Fn(&SharedString) -> Result<(), SharedString>>;

type OkActionFn = Rc<dyn Fn(Option<SharedString>, &mut Window, &mut App)>;

type CancelActionFn = Rc<dyn Fn(&mut Window, &mut App)>;

/// Which of the four alerts this is: whether there is an input field, and whether there is a
/// cancel button. An alert carries no other shape.
#[derive(Copy, Clone)]
enum AlertKind {
    Ok,
    OkCancel,
    InputOk,
    InputOkCancel,
}

impl AlertKind {
    fn with_cancel(&self) -> bool {
        matches!(self, AlertKind::OkCancel | AlertKind::InputOkCancel)
    }
}

static ALERT_ID: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// The shape, the copy and the buttons of an [`Alert`], while they are still being chosen.
///
/// This is the builder [`LayerExt::alert`] hands to the closure that opens an alert. An alert is one
/// of four combinations of an optional input field and an ok/cancel button row, and nothing else:
///
/// | Constructor                       | Shape                             |
/// | --------------------------------- | --------------------------------- |
/// | [`AlertBuilder::ok`]              | title, message, ok                |
/// | [`AlertBuilder::ok_cancel`]       | title, message, ok, cancel        |
/// | [`AlertBuilder::input_ok`]        | title, message, input, ok         |
/// | [`AlertBuilder::input_ok_cancel`] | title, message, input, ok, cancel |
///
/// Choosing is not drawing: [`AlertBuilder::build`] turns these choices into the [`Alert`] the layer
/// draws, in the one step of [`LayerExt::alert`] that has a context to make entities in and that runs
/// before any frame. What the builder holds is therefore all the caller asked for, and the alert it
/// builds holds all that is drawn.
pub struct AlertBuilder {
    kind: AlertKind,
    title: SharedString,
    message: SharedString,
    input: Option<Entity<InputState>>,
    ok_text: SharedString,
    ok_action: Option<OkActionFn>,
    cancel_text: SharedString,
    cancel_action: Option<CancelActionFn>,
    validate: Option<ValidatorFn>,
}

impl AlertBuilder {
    /// An alert whose shape, title and message are still to be chosen.
    pub(crate) fn new() -> Self {
        Self {
            kind: AlertKind::Ok,
            title: SharedString::default(),
            message: SharedString::default(),
            input: None,
            ok_text: SharedString::new_static("OK"),
            ok_action: None,
            cancel_text: SharedString::new_static("Cancel"),
            cancel_action: None,
            validate: None,
        }
    }

    /// A title, a message and an ok button.
    pub fn ok(mut self, title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        self.kind = AlertKind::Ok;
        self.title = title.into();
        self.message = message.into();
        self
    }

    /// A title, a message, an ok button and a cancel button.
    pub fn ok_cancel(
        mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
    ) -> Self {
        self.kind = AlertKind::OkCancel;
        self.title = title.into();
        self.message = message.into();
        self
    }

    /// A title, a message, an input field and an ok button.
    pub fn input_ok(
        mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        input: Entity<InputState>,
    ) -> Self {
        self.kind = AlertKind::InputOk;
        self.title = title.into();
        self.message = message.into();
        self.input(input)
    }

    /// A title, a message, an input field, an ok button and a cancel button.
    pub fn input_ok_cancel(
        mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        input: Entity<InputState>,
    ) -> Self {
        self.kind = AlertKind::InputOkCancel;
        self.title = title.into();
        self.message = message.into();
        self.input(input)
    }

    fn input(mut self, input: Entity<InputState>) -> Self {
        self.input = Some(input);
        self
    }

    /// Customizes the text of the ok button.
    pub fn ok_text(mut self, ok_text: impl Into<SharedString>) -> Self {
        self.ok_text = ok_text.into();
        self
    }

    /// Customizes the text of the cancel button.
    pub fn cancel_text(mut self, cancel_text: impl Into<SharedString>) -> Self {
        self.cancel_text = cancel_text.into();
        self
    }

    /// Runs after the alert has been dismissed by the ok button. The value is what the input field
    /// held, and is `None` for the alerts that have no input field.
    pub fn ok_action(
        mut self,
        ok_action: impl Fn(Option<SharedString>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.ok_action = Some(Rc::new(ok_action));
        self
    }

    /// Runs after the alert has been dismissed by the cancel button.
    pub fn cancel_action(
        mut self,
        cancel_action: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.cancel_action = Some(Rc::new(cancel_action));
        self
    }

    /// Checks the value of the input field when the ok button is clicked. `Err` blocks the
    /// dismissal and shows its message under the field until the value passes. Only meaningful on
    /// the alerts that have an input field.
    pub fn validate(
        mut self,
        validate: impl Fn(&SharedString) -> Result<(), SharedString> + 'static,
    ) -> Self {
        self.validate = Some(Rc::new(validate));
        self
    }

    /// Builds the alert: its identity, the markdown state its message is drawn from, and every
    /// choice this builder holds.
    ///
    /// The markdown state is the one thing that cannot be made where the message is chosen: those
    /// constructors run on the builder, which is not an entity and has no context to make one in. It
    /// is made here, in the step that builds the alert itself, so that an [`Alert`] is complete the
    /// moment it exists — nothing about it is left for a frame to fill in. It carries its own
    /// identity, which is what its buttons are keyed by, so nothing outside has to hand it one.
    pub(crate) fn build(self, cx: &mut Context<Alert>) -> Alert {
        Alert {
            id: ElementId::NamedInteger(
                SharedString::new_static("alert"),
                ALERT_ID.fetch_add(1, Ordering::Relaxed),
            ),
            kind: self.kind,
            title: self.title,
            message_markdown: cx.new(|cx| MarkdownState::new(self.message, cx)),
            input: self.input,
            ok_text: self.ok_text,
            ok_action: self.ok_action,
            cancel_text: self.cancel_text,
            cancel_action: self.cancel_action,
            validate: self.validate,
            error: None,
        }
    }
}

/// The title + message + input + buttons specialization of [`Dialog`](crate::dialog::Dialog), as the
/// layer draws it.
///
/// It is what [`AlertBuilder::build`] makes, and it holds everything it draws: the markdown state
/// its message is drawn from, and — for the alerts that have a field — the field's own entity, both
/// made as the alert was built rather than on a frame it is drawn in.
///
/// The layout is a column of title, message, input and the button row. The button row carries no
/// padding and no margin: a lone button fills the panel, while two buttons share it through
/// [`Join`].
///
/// The message and the refusal under the field are drawn by the markdown component, so both carry
/// its semantics: the text is parsed as CommonMark and laid out left-aligned, and the constructs
/// that component drops — lists, quotes, fenced code blocks — draw nothing at all. A message is
/// therefore plain copy, and one written as a list or a code block shows as an empty line.
///
/// This is the content entity of the dialog [`LayerExt::alert`] mounts on the layer's alert slot:
/// the builder that slot is given runs once, before any frame is drawn, and the alert it builds is
/// what every frame draws. Everything the alert needs across those frames is therefore its own
/// state, the validation error above all.
///
/// It closes itself through [`LayerExt::dismiss_alert`], which takes the alert off the layer and
/// leaves nothing of it to clean up. It deliberately does not emit
/// [`gpui_kit::DismissEvent`] the way a general dialog's content does: an alert closes itself
/// through the layer's alert slot instead, and one special case in the mount for one kind of content
/// is not worth the asymmetry.
///
/// The backdrop, the panel and the enter/exit transition are the dialog layer's.
pub struct Alert {
    id: ElementId,
    kind: AlertKind,
    title: SharedString,
    /// The message as the markdown state the panel draws, built by [`AlertBuilder::build`].
    message_markdown: Entity<MarkdownState>,
    input: Option<Entity<InputState>>,
    ok_text: SharedString,
    ok_action: Option<OkActionFn>,
    cancel_text: SharedString,
    cancel_action: Option<CancelActionFn>,
    validate: Option<ValidatorFn>,
    /// What the last refused value was turned down with, as markdown, drawn under the input field.
    /// Being `Some` is the refusal itself: the alert draws it until a value passes, which drops the
    /// state with the refusal rather than keeping it around to be cleared.
    error: Option<Entity<MarkdownState>>,
}

impl Render for Alert {
    /// Draws the alert itself: a column of title, markdown message, input with the markdown refusal
    /// under it, and the button row.
    ///
    /// Nothing here builds an entity: every one it draws was made as the alert was built, which is
    /// what keeps the frame it is drawn in from reaching back into its construction.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = theme.roles.fg.default.hsla;
        let error_color = theme.roles.status.danger.base.hsla;
        let id = self.id.clone();

        let ok_button = Button::new((id.clone(), "ok"))
            .justify_center()
            .child(self.ok_text.clone())
            .debug_selector(|| "BASE_UI_ALERT_OK".into())
            // The alert is where the value is checked, so a refusal leaves the error on the alert
            // itself and returns: the entity is redrawn with it and stays open. Only a value that
            // passes dismisses the alert, and the ok action sees the value that was checked.
            .on_click(cx.listener(|alert, _, window, cx| {
                let value = alert.input.as_ref().map(|input| input.read(cx).value(cx));

                if let Some(validate) = alert.validate.as_ref() {
                    let checked = value.clone().unwrap_or_default();

                    if let Err(message) = validate(&checked) {
                        // The refusal is turned into its own markdown state rather than written
                        // over the last one: a refusal only happens on a click, and a state that
                        // is replaced is one nothing has to keep in step with the text it shows.
                        alert.error = Some(cx.new(|cx| MarkdownState::new(message, cx)));
                        cx.notify();

                        return;
                    }
                }

                alert.error = None;
                window.dismiss_alert(cx);

                if let Some(ok_action) = alert.ok_action.as_ref() {
                    ok_action(value, window, cx);
                }
            }));

        let cancel_button = Button::new((id, "cancel"))
            .child(self.cancel_text.clone())
            .tonal()
            .debug_selector(|| "BASE_UI_ALERT_CANCEL".into())
            .on_click(cx.listener(|alert, _, window, cx| {
                window.dismiss_alert(cx);

                if let Some(cancel_action) = alert.cancel_action.as_ref() {
                    cancel_action(window, cx);
                }
            }));

        // No padding, no margin: a lone button fills the panel, and two buttons share it through
        // the join that squares their shared edge.
        let buttons = if self.kind.with_cancel() {
            Join::default()
                .button(ok_button.justify_center().flex_1())
                .button(cancel_button.justify_center().flex_1())
                .flex_1()
                .into_any_element()
        } else {
            ok_button.w_full().into_any_element()
        };

        div()
            .debug_selector(|| "BASE_UI_ALERT".into())
            .w_96()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_hidden()
            .child(
                div()
                    .text_center()
                    .text_color(content)
                    .font_bold()
                    .text_xl()
                    .child(self.title.clone()),
            )
            .child(Markdown::new(&self.message_markdown).text_color(content))
            .when_some(self.input.clone(), |this, input| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(Input::new(&input))
                        .when_some(self.error.clone(), |this, error| {
                            this.child(
                                div()
                                    .debug_selector(|| "BASE_UI_ALERT_ERROR".into())
                                    .text_sm()
                                    .text_color(error_color)
                                    .child(Markdown::new(&error)),
                            )
                        }),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .items_stretch()
                    .when(self.kind.with_cancel(), |this| this.flex_1())
                    .child(buttons),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc, time::Duration};

    use gpui_kit::{
        AppContext as _, Context, IntoElement, Modifiers, Render, SharedString, Styled as _,
        TestAppContext, VisualTestContext, Window, div,
    };

    use crate::{
        alert::AlertBuilder,
        animation::ANIMATION_TICK,
        input::InputStateBuilder,
        layer::{Layer, LayerExt as _},
        style,
    };

    const PANEL: &str = "BASE_UI_ALERT";
    const OK: &str = "BASE_UI_ALERT_OK";
    const CANCEL: &str = "BASE_UI_ALERT_CANCEL";
    const ERROR: &str = "BASE_UI_ALERT_ERROR";

    /// What an alert's buttons handed back, so that a test reads the value the way a caller
    /// does.
    #[derive(Default)]
    struct Reported {
        ok: Option<Option<SharedString>>,
        cancel: usize,
    }

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    /// A window whose root is a [`Layer`], the way an application mounts one, so that the
    /// alert has a window layer to be opened on.
    fn harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(style::theme::install);
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        cx.run_until_parked();

        cx
    }

    /// Lets the alert's own fade finish and repaints, so that an alert in its first frames —
    /// painted at zero opacity — is not mistaken for one that stays.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the alert should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);
    }

    fn open_ok_cancel(cx: &mut VisualTestContext, reported: Rc<RefCell<Reported>>) {
        cx.update(|window, cx| {
            window.alert(cx, move |alert, _, _| {
                alert
                    .ok_cancel("Discard changes", "Your unsaved changes will be lost.")
                    .ok_text("Discard")
                    .cancel_text("Keep editing")
                    .ok_action({
                        let reported = reported.clone();

                        move |value, _, _| {
                            reported.borrow_mut().ok = Some(value);
                        }
                    })
                    .cancel_action(move |_, _| reported.borrow_mut().cancel += 1)
            });
        });
        settle(cx);
    }

    /// The two-button alert is one ok and one cancel, and each of them reports what it was
    /// clicked on rather than the other.
    #[gpui_kit::test]
    fn an_alert_with_a_cancel_button_should_report_which_button_was_clicked(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx);
        let reported = Rc::new(RefCell::new(Reported::default()));
        open_ok_cancel(cx, reported.clone());

        assert_eq!(reported.borrow().ok, None);
        assert_eq!(reported.borrow().cancel, 0);

        click(cx, OK);

        assert_eq!(
            reported.borrow().ok,
            Some(None),
            "the ok action should run with no value, because this alert has no input field"
        );
        assert_eq!(reported.borrow().cancel, 0);
    }

    /// The cancel button dismisses the alert without running the ok action.
    #[gpui_kit::test]
    fn the_cancel_button_should_dismiss_without_running_the_ok_action(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let reported = Rc::new(RefCell::new(Reported::default()));
        open_ok_cancel(cx, reported.clone());

        click(cx, CANCEL);

        assert_eq!(reported.borrow().cancel, 1);
        assert_eq!(reported.borrow().ok, None);
        assert!(
            cx.debug_bounds(PANEL).is_none(),
            "a dismissed alert should leave the window layer once its exit transition is over"
        );
    }

    /// An alert opened without a cancel button draws one button, and that button fills the
    /// panel, because the button row carries no padding.
    #[gpui_kit::test]
    fn an_alert_without_a_cancel_button_should_draw_one_filling_button(cx: &mut TestAppContext) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            window.alert(cx, |alert, _, _| {
                alert.ok("Saved", "The file has been written.")
            });
        });
        settle(cx);

        let ok = cx.debug_bounds(OK).expect("the ok button should be drawn");
        let panel = cx.debug_bounds(PANEL).expect("the panel should be drawn");

        assert!(
            cx.debug_bounds(CANCEL).is_none(),
            "an alert opened without a cancel button should not draw one"
        );
        assert_eq!(
            ok.size.width, panel.size.width,
            "a lone ok button should fill the panel"
        );
    }

    /// The two buttons an alert with a cancel button draws share the panel between them,
    /// through the join that squares their shared edge.
    #[gpui_kit::test]
    fn two_buttons_should_share_the_panel(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let reported = Rc::new(RefCell::new(Reported::default()));
        open_ok_cancel(cx, reported);

        let ok = cx.debug_bounds(OK).expect("the ok button should be drawn");
        let cancel = cx
            .debug_bounds(CANCEL)
            .expect("the cancel button should be drawn");
        let panel = cx.debug_bounds(PANEL).expect("the panel should be drawn");

        assert_eq!(
            ok.size.width + cancel.size.width,
            panel.size.width,
            "two buttons should fill the panel between them, with no padding in the button row"
        );
        assert_eq!(
            ok.origin.x, panel.origin.x,
            "the ok button should sit on the left edge of the panel"
        );
        assert_eq!(
            cancel.origin.x + cancel.size.width,
            panel.origin.x + panel.size.width,
            "the cancel button should sit on the right edge of the panel"
        );
    }

    /// An alert is complete the moment it is built: the markdown state its message is drawn from
    /// exists before any frame draws it, which is what keeps building an alert out of the frame.
    #[gpui_kit::test]
    fn an_alert_should_be_complete_when_it_is_built(cx: &mut TestAppContext) {
        let alert = cx.update(|cx| {
            cx.new(|cx| {
                AlertBuilder::new()
                    .ok("Saved", "**The file** has been written.")
                    .build(cx)
            })
        });

        assert_eq!(
            cx.read(|cx| alert
                .read(cx)
                .message_markdown
                .read(cx)
                .source()
                .to_string()),
            "**The file** has been written.",
            "the message should be parsed into the alert as it is built, rather than by a frame that \
             draws it"
        );
    }

    /// The message is drawn by the markdown component rather than as one unbroken line: the syntax
    /// it is written in is interpreted, and the paragraph that leaves is what the panel holds.
    #[gpui_kit::test]
    fn the_message_should_be_drawn_by_the_markdown_component(cx: &mut TestAppContext) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            window.alert(cx, |alert, _, _| {
                alert.ok("Saved", "**The file** has been written.")
            });
        });
        settle(cx);

        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN").is_some(),
            "the message should be drawn by the markdown component"
        );
        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_some(),
            "the paragraph the message parses to should be drawn"
        );
    }

    /// What the markdown component drops is dropped here too: the alert hands its message over
    /// rather than drawing the raw text beside it.
    #[gpui_kit::test]
    fn a_message_the_markdown_component_drops_should_draw_no_paragraph(cx: &mut TestAppContext) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            window.alert(cx, |alert, _, _| alert.ok("Saved", "- item"));
        });
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "the alert should still be drawn around the message"
        );
        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").is_none(),
            "a list is dropped whole by the markdown component, so it should draw no paragraph"
        );
    }

    /// The refusal under the field is markdown as well. Its message here is a construct markdown
    /// drops, so the only paragraph the alert can draw is the refusal's: that the paragraph inside
    /// the line under the field is drawn is what says the refusal went through the component, since
    /// a refusal drawn as plain text would leave none.
    #[gpui_kit::test]
    fn the_refusal_should_be_drawn_by_the_markdown_component(cx: &mut TestAppContext) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            window.alert(cx, |alert, window, cx| {
                let input = cx.new(|cx| InputStateBuilder::new("alert-input").build(window, cx));

                alert
                    .input_ok("New folder", "- dropped", input)
                    .validate(|_| Err(SharedString::new_static("**A name is required.**")))
            });
        });
        settle(cx);

        click(cx, OK);

        let error = cx
            .debug_bounds(ERROR)
            .expect("the refusal should be drawn under the field");
        let paragraph = cx.debug_bounds("BASE_UI_MARKDOWN_BLOCK_0").expect(
            "the refusal should be drawn by the markdown component, and a refusal drawn as plain \
             text would leave no paragraph to draw",
        );

        assert!(
            error.contains(&paragraph.center()),
            "the paragraph should be the one drawn inside the line the refusal is drawn on"
        );
    }

    /// The ok action of an alert with an input field receives what the field holds.
    #[gpui_kit::test]
    fn the_ok_action_of_an_input_alert_should_receive_the_value(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let reported = Rc::new(RefCell::new(Reported::default()));

        cx.update(|window, cx| {
            let reported = reported.clone();

            window.alert(cx, move |alert, window, cx| {
                let input = cx.new(|cx| {
                    let mut state = InputStateBuilder::new("alert-input").build(window, cx);

                    state.update_value("old.txt", window, cx);

                    state
                });

                alert
                    .input_ok("Rename", "Enter the new name.", input)
                    .ok_text("Rename")
                    .ok_action(move |value, _, _| {
                        reported.borrow_mut().ok = Some(value);
                    })
            });
        });
        settle(cx);

        click(cx, OK);

        assert_eq!(
            reported.borrow().ok,
            Some(Some(SharedString::new_static("old.txt"))),
            "the ok action should receive the value the field started with, and no input yet"
        );
    }

    /// An input alert draws its field across the whole panel, which is the width the title and
    /// the message are drawn in: the panel carries no horizontal padding of its own.
    #[gpui_kit::test]
    fn the_input_of_an_alert_should_fill_the_panel(cx: &mut TestAppContext) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            window.alert(cx, |alert, window, cx| {
                let input = cx.new(|cx| InputStateBuilder::new("alert-input").build(window, cx));

                alert.input_ok("Rename", "Enter the new name.", input)
            });
        });
        settle(cx);

        let panel = cx.debug_bounds(PANEL).expect("the panel should be drawn");
        let input = cx
            .debug_bounds("BASE_UI_INPUT")
            .expect("the input should be drawn");

        assert_eq!(
            input.size.width, panel.size.width,
            "the field should span the panel, which carries no horizontal padding"
        );
    }

    /// A validator that refuses the value keeps the alert open, and the alert stays until a
    /// value it accepts is confirmed.
    #[gpui_kit::test]
    fn a_refused_value_should_keep_the_alert_open(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let reported = Rc::new(RefCell::new(Reported::default()));

        cx.update(|window, cx| {
            let reported = reported.clone();

            window.alert(cx, move |alert, window, cx| {
                let input = cx.new(|cx| InputStateBuilder::new("alert-input").build(window, cx));

                alert
                    .input_ok_cancel("New folder", "The name may not be empty.", input)
                    .validate(|value| {
                        if value.is_empty() {
                            Err(SharedString::new_static("A name is required."))
                        } else {
                            Ok(())
                        }
                    })
                    .ok_action(move |value, _, _| {
                        reported.borrow_mut().ok = Some(value);
                    })
            });
        });
        settle(cx);

        click(cx, OK);

        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "a refused value should leave the alert open"
        );
        assert_eq!(
            reported.borrow().ok,
            None,
            "a refused value should not run the ok action"
        );

        click(cx, CANCEL);

        assert!(cx.debug_bounds(PANEL).is_none());
    }

    /// A refusal belongs to the alert that made it: the message is the alert entity's own field
    /// rather than state hung on the window, so the alert mounted after it starts with no error,
    /// however alike the two are.
    #[gpui_kit::test]
    fn a_refused_value_should_stay_with_the_alert_that_refused_it(cx: &mut TestAppContext) {
        let cx = harness(cx);

        cx.update(|window, cx| {
            window.alert(cx, |alert, window, cx| {
                let input = cx.new(|cx| InputStateBuilder::new("alert-input").build(window, cx));

                alert
                    .input_ok_cancel("New folder", "The name may not be empty.", input)
                    .validate(|value| {
                        if value.is_empty() {
                            Err(SharedString::new_static("A name is required."))
                        } else {
                            Ok(())
                        }
                    })
            });
        });
        settle(cx);

        click(cx, OK);

        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "a refused value should leave the alert open"
        );
        assert!(
            cx.debug_bounds(ERROR).is_some(),
            "the refusal should be drawn under the field of the alert that refused the value"
        );

        // Another alert of the same shape, so that the line the refusal was drawn on is a line this
        // one draws as well: what it must not show is the refusal of the alert before it.
        cx.update(|window, cx| {
            window.alert(cx, |alert, window, cx| {
                let input = cx.new(|cx| InputStateBuilder::new("alert-input").build(window, cx));

                alert.input_ok("Rename", "Enter the new name.", input)
            });
        });
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL).is_some() && cx.debug_bounds("BASE_UI_INPUT").is_some(),
            "the alert opened over the one that refused should be drawn with its own field"
        );
        assert!(
            cx.debug_bounds(ERROR).is_none(),
            "the alert opened over the one that refused should show no error: a refusal is the \
             refusing alert's own state and must not outlive it"
        );
    }
}

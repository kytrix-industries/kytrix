use gpui_kit::{prelude::FluentBuilder, *};

use crate::{
    alert::{Alert, AlertBuilder},
    dialog::Dialog,
};

/// The window's dialog layer: the view at the bottom, the dialogs opened over it, and the alert
/// over all of them.
///
/// That is what "the alert is above every dialog" means: the layer's own stacking order. A popup —
/// a dropdown, a tooltip, an avatar menu, anything drawn by a [`crate::overlay`] panel — is painted
/// by GPUI after the layer it was opened from, so an open popup is above the alert as well.
pub struct Layer {
    view: AnyView,
    /// The dialogs on this layer, oldest first, which is also the order they are drawn in: a dialog
    /// opened later is painted over the dialogs it was opened over.
    ///
    /// Hit testing walks the hitboxes of a frame in reverse, and the dialog root's
    /// `HitboxBehavior::BlockMouse` hitbox stops that walk, so the dialog drawn last — the one
    /// opened last — is also the one the mouse reaches, and the dialogs under it are not.
    dialogs: Vec<Entity<Dialog>>,
    /// The alert: at most one, on the layer above every dialog.
    alert: Option<Entity<Dialog>>,
}

impl Layer {
    pub fn new(view: impl Into<AnyView>) -> Self {
        Layer {
            view: view.into(),
            dialogs: Vec::new(),
            alert: None,
        }
    }
}

impl Render for Layer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Children are painted in the order they are added, so this is the stacking order: the
        // view first, then the dialogs oldest first, then the alert over all of them.
        div()
            .relative()
            .size_full()
            .child(div().absolute().inset_0().child(self.view.clone()))
            .children(
                self.dialogs
                    .iter()
                    .map(|dialog| div().absolute().inset_0().child(dialog.clone())),
            )
            .when_some(self.alert.as_ref(), |this, alert| {
                this.child(div().absolute().inset_0().child(alert.clone()))
            })
    }
}

pub trait LayerExt {
    /// Opens a dialog whose content is `content`, over every dialog already showing: it is drawn on
    /// top of them, and it is the one a dismissal reaches. The layer keeps as many dialogs as are
    /// opened.
    ///
    /// `content` is a view entity the caller made and keeps a handle of its own to; the dialog
    /// holds a second, strong reference for as long as it is on the layer, so the content is
    /// released — and its state with it — once the dialog has left. Nothing has to clean that state
    /// up, and nothing outside the content has to know how.
    ///
    /// The dialog subscribes to the content's [`DismissEvent`], so the content closes itself
    /// wherever it has a context to say so from — a click handler, a key handler, a task that
    /// finished — with `cx.emit(DismissEvent)`, without needing a window and without depending on
    /// being the dialog on top. That is the only way a dialog closes: the layer opens dialogs and
    /// takes them off, and nothing outside a content decides when it is done.
    ///
    /// The dialog is pushed at the end of the effect cycle rather than where this is called, and
    /// that is what makes it drawn: the caller is inside a frame that is being drawn as often as
    /// not — a dialog opened by another dialog's content, a form opened by the page that asked for
    /// it — and a notification made while a frame is drawing does not mark the window dirty, so a
    /// dialog pushed there would sit on the layer with no frame ever drawing it.
    ///
    /// A dismissal issued before that push has run therefore reaches the dialog that was on top
    /// before this one, not the one being opened.
    fn dialog<Content>(&self, cx: &mut App, content: Entity<Content>)
    where
        Content: Render + EventEmitter<DismissEvent>;

    /// Removes `dialog` from the layer wherever it sits on it, and does nothing when it is not on
    /// the layer. Called by [`Dialog`] once its exit transition has finished, which is why it takes
    /// the dialog that finished rather than the one on top: with several dialogs open, the one that
    /// has just left is not necessarily the one on top, nor the only one.
    ///
    /// This is where the last strong reference to the dialog's content is dropped, and with it the
    /// content itself.
    fn clean_dialog(&self, dialog: &Entity<Dialog>, cx: &mut App);

    /// Opens an alert with `builder`, on the layer above every dialog: it is drawn over all of them
    /// and takes the mouse from them. It drops the caret the same way a dialog does, because it is
    /// modal too.
    ///
    /// `builder` is called once, at the end of the effect cycle this is called in — before any frame
    /// draws the alert — and [`AlertBuilder::build`] then turns what it returns into the [`Alert`]
    /// entity every frame draws from then on, which is what lets the alert keep its own state, the
    /// validation error above all. The builder is handed the [`Context<Alert>`] of the entity it is
    /// about to become, so it can make the input entity an input alert owns.
    ///
    /// It is called where the alert is made rather than while the dialog draws it, because building
    /// an alert makes the entities it is drawn from — the field's, and the markdown state of its
    /// message — and an entity made in the middle of a frame is the re-entrancy this layer keeps out
    /// of its rendering.
    ///
    /// There is one alert at a time, so an alert opened over another one replaces it rather than
    /// stacking over it. Like [`LayerExt::dialog`], the alert is pushed at the end of the effect
    /// cycle, which is what makes it drawn even when its caller is inside a frame that is being
    /// drawn.
    fn alert(
        &self,
        cx: &mut App,
        builder: impl FnOnce(AlertBuilder, &mut Window, &mut Context<Alert>) -> AlertBuilder + 'static,
    );

    /// Dismisses the alert: it plays its exit transition and is then removed. A layer with no
    /// alert is left alone.
    ///
    /// This is how an [`Alert`] closes itself, from either of its buttons.
    fn dismiss_alert(&self, cx: &mut App);

    /// Removes `alert` from the layer when it is still the alert the layer holds, and does nothing
    /// otherwise. Called by the alert once its exit transition has finished; the alert is named
    /// rather than the slot cleared so that the layer only ever drops the alert it is showing.
    ///
    /// This is where the last strong reference to the [`Alert`] entity is dropped.
    fn clean_alert(&self, alert: &Entity<Dialog>, cx: &mut App);
}

impl LayerExt for Window {
    fn dialog<Content>(&self, cx: &mut App, content: Entity<Content>)
    where
        Content: Render + EventEmitter<DismissEvent>,
    {
        // The typed handle the subscription below needs, and the erased one the dialog stores: the
        // subscription entry points all take a concrete `Entity<T>`, and `AnyView` can only hand
        // back an `EntityId`.
        let erased: AnyView = content.clone().into();

        // The dialog is made here rather than in the deferred push below, and that is what makes an
        // emit sent later in this same effect cycle reach it. A subscription is inert until the
        // effect cycle that activates it runs (`App::new_subscription` defers the activation), and
        // that activation is queued where `subscribe` is called: made here, it is queued ahead of
        // whatever the caller does next — a content asked to open and then told to close again, a
        // task that finishes in the same turn — and the emit is delivered. Made inside the deferred
        // push, it would be queued behind that emit and the close would be dropped on the floor.
        //
        // Only the push is deferred, and the reason for that is unchanged: see below.
        let dialog = cx.new(|cx| {
            // The content says it is done by emitting, and the subscription dies with the dialog:
            // nobody has to take it down, and a content that outlives its dialog cannot reach it.
            cx.subscribe(&content, |dialog: &mut Dialog, _, _: &DismissEvent, cx| {
                if dialog.is_open() {
                    dialog.close();
                    cx.notify();
                }
            })
            .detach();

            Dialog::new(erased)
        });

        self.defer(cx, move |window, cx| {
            let Some(Some(layer)) = window.root::<Layer>() else {
                panic!("no layer")
            };

            layer.update(cx, |this, cx| {
                this.dialogs.push(dialog);
                cx.notify();
            });
        });
    }

    fn clean_dialog(&self, dialog: &Entity<Dialog>, cx: &mut App) {
        let Some(Some(layer)) = self.root::<Layer>() else {
            return;
        };

        layer.update(cx, |this, cx| {
            let opened = this.dialogs.len();
            this.dialogs.retain(|open| open != dialog);

            if this.dialogs.len() != opened {
                cx.notify();
            }
        });
    }

    fn alert(
        &self,
        cx: &mut App,
        builder: impl FnOnce(AlertBuilder, &mut Window, &mut Context<Alert>) -> AlertBuilder + 'static,
    ) {
        // The alert is built here, where a `Window` and a `Context<Alert>` are both in hand, and not
        // while the dialog draws it: building an alert makes the entities it is drawn from — the
        // field's, and the markdown state of its message — and creating an entity in the middle of a
        // frame is what this avoids. `AlertBuilder::build` is where that happens, so what the builder
        // returns is finished: the dialog has nothing to build and draws the entity it was given.
        self.defer(cx, move |window, cx| {
            let alert = cx.new(|cx| builder(AlertBuilder::new(), window, cx).build(cx));
            let dialog = cx.new(|_| Dialog::alert(alert));

            let Some(Some(layer)) = window.root::<Layer>() else {
                panic!("no layer")
            };

            layer.update(cx, |this, cx| {
                this.alert = Some(dialog);
                cx.notify();
            });
        });
    }

    fn dismiss_alert(&self, cx: &mut App) {
        let Some(Some(layer)) = self.root::<Layer>() else {
            return;
        };

        layer.update(cx, |this, cx| {
            let Some(alert) = this.alert.clone() else {
                return;
            };

            alert.update(cx, |dialog, cx| {
                dialog.close();
                cx.notify();
            });
        });
    }

    fn clean_alert(&self, alert: &Entity<Dialog>, cx: &mut App) {
        let Some(Some(layer)) = self.root::<Layer>() else {
            return;
        };

        layer.update(cx, |this, cx| {
            if this.alert.as_ref() != Some(alert) {
                return;
            }

            this.alert = None;
            cx.notify();
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Duration,
    };

    use gpui_kit::{
        AppContext as _, Context, DismissEvent, EmptyView, Entity, EntityId, EventEmitter,
        FocusHandle, InteractiveElement as _, IntoElement, Modifiers, ParentElement as _, Render,
        StatefulInteractiveElement as _, Styled as _, TestAppContext, VisualTestContext, Window,
        px,
    };

    use crate::{
        animation::ANIMATION_TICK,
        button::Button,
        input::{Input, InputState, InputStateBuilder},
        layer::{Layer, LayerExt as _},
        style,
    };

    const DIALOG_A: &str = "LAYER_DIALOG_A";
    const DIALOG_B: &str = "LAYER_DIALOG_B";
    const STACK: [&str; 4] = [
        "LAYER_STACK_0",
        "LAYER_STACK_1",
        "LAYER_STACK_2",
        "LAYER_STACK_3",
    ];
    const ALERT: &str = "BASE_UI_ALERT";
    const ALERT_OK: &str = "BASE_UI_ALERT_OK";
    const ALERT_CANCEL: &str = "BASE_UI_ALERT_CANCEL";
    const FOCUS_PROBE: &str = "layer-focus-probe";
    const KEYED_SCOPE_PROBE: &str = "layer-scope-probe";

    /// A dialog whose content is a single button counting the clicks that reach it, which is how a
    /// test tells which of several stacked dialogs the mouse reached.
    ///
    /// The counter lives in the content, which is what a dialog hosts: the dialog itself never sees
    /// it, and it is released with the content.
    struct CountingButton {
        selector: &'static str,
        clicks: Rc<Cell<usize>>,
        /// Whether the button is drawn large enough to occupy the middle of the window, which is
        /// where an alert draws its own button as well.
        wide: bool,
    }

    impl EventEmitter<DismissEvent> for CountingButton {}

    impl Render for CountingButton {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let selector = self.selector;
            let clicks = self.clicks.clone();

            let button = Button::new(selector)
                .child("Click")
                .debug_selector(move || selector.into())
                .on_click(move |_, _, _| clicks.set(clicks.get() + 1));

            if self.wide {
                button.w(px(800.0)).h(px(600.0))
            } else {
                button
            }
        }
    }

    /// A dialog whose content asks the window for keyed state on every frame, recording the entity
    /// it is handed on each of them.
    ///
    /// Every frame is recorded rather than the first one, because what a stacked dialog has to keep
    /// is its state *while another dialog is drawn over it*: a dialog that stopped being drawn would
    /// stop recording, and one that was handed the other's state would record the other's id.
    struct KeyedStateProbe {
        recorded: Rc<RefCell<Vec<EntityId>>>,
    }

    impl EventEmitter<DismissEvent> for KeyedStateProbe {}

    impl Render for KeyedStateProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let state = window.use_keyed_state(KEYED_SCOPE_PROBE, cx, |_, _| 0usize);
            self.recorded.borrow_mut().push(state.entity_id());

            Button::new("keyed-state-probe").child("Probe")
        }
    }

    /// A dialog whose content records the entity it is on for every frame it draws, which is how a
    /// test tells that the dialog draws the entity it was opened with on all of them.
    struct IdentityProbe {
        drawn: Rc<RefCell<Vec<EntityId>>>,
    }

    impl EventEmitter<DismissEvent> for IdentityProbe {}

    impl Render for IdentityProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.drawn.borrow_mut().push(cx.entity_id());

            Button::new("identity-probe").child("Probe")
        }
    }

    /// A dialog whose content is an input field, so that a test can watch the caret a dialog takes
    /// out of what was behind it.
    struct InputContent {
        input: Entity<InputState>,
    }

    impl EventEmitter<DismissEvent> for InputContent {}

    impl Render for InputContent {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Input::new(&self.input)
        }
    }

    /// A dialog whose content takes the caret on the first frame it draws, which is how a test tells
    /// a caret the dialog took for its content from one it took straight back out of it.
    struct FocusTakingContent {
        focus: FocusHandle,
        focused: Rc<Cell<bool>>,
    }

    impl EventEmitter<DismissEvent> for FocusTakingContent {}

    impl Render for FocusTakingContent {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            // Only on the first frame, so what the assertion sees is the focus this took rather
            // than a caret that was taken again and again.
            if !self.focused.replace(true) {
                window.focus(&self.focus, cx);
            }

            Button::new("focus-taking-content")
                .child("Focus")
                .debug_selector(|| DIALOG_A.into())
        }
    }

    /// A dialog whose content opens another dialog while it draws itself, which only reaches the
    /// layer because the push is deferred to the end of the effect cycle.
    struct NestedDialogOpener {
        opened: Rc<Cell<bool>>,
    }

    impl EventEmitter<DismissEvent> for NestedDialogOpener {}

    impl Render for NestedDialogOpener {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.opened.replace(true) {
                let nested = cx.new(|_| CountingButton {
                    selector: DIALOG_B,
                    clicks: Rc::new(Cell::new(0)),
                    wide: false,
                });

                window.dialog(cx, nested);
            }

            Button::new("outer-dialog")
                .child("Outer")
                .debug_selector(|| DIALOG_A.into())
        }
    }

    /// A dialog whose content closes its dialog by emitting the event the dialog subscribes to when
    /// its button is clicked, and which keeps its handle so that a test can make it emit again
    /// whenever it likes rather than only by clicking.
    ///
    /// Counting the clicks as well as the emits is what tells a click that reached the content from
    /// an emit made by the test on the content's behalf.
    struct DismissingContent {
        selector: &'static str,
        clicks: Rc<Cell<usize>>,
    }

    impl EventEmitter<DismissEvent> for DismissingContent {}

    impl Render for DismissingContent {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let selector = self.selector;

            Button::new(selector)
                .child("Click")
                .debug_selector(move || selector.into())
                .on_click(cx.listener(|content, _, _, cx| {
                    content.clicks.set(content.clicks.get() + 1);
                    cx.emit(DismissEvent);
                }))
        }
    }

    /// A dialog whose content closes its own dialog by emitting the event the dialog subscribes to,
    /// counting the emits so that a test can say how often the content asked.
    struct EmittingContent {
        selector: &'static str,
        emitted: Rc<Cell<usize>>,
    }

    impl EventEmitter<DismissEvent> for EmittingContent {}

    impl Render for EmittingContent {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let selector = self.selector;

            Button::new(selector)
                .child("Close")
                .debug_selector(move || selector.into())
                .on_click(cx.listener(|content, _, _, cx| {
                    content.emitted.set(content.emitted.get() + 1);
                    cx.emit(DismissEvent);
                }))
        }
    }

    /// A dialog whose content leaves a mark when it is dropped, which is how a test tells that the
    /// layer released the content rather than leaking it.
    ///
    /// Its button closes the dialog the way every content does, so what a test watches is the
    /// release the ordinary close path leaves behind.
    struct ReleasingContent {
        dropped: Rc<Cell<bool>>,
    }

    impl EventEmitter<DismissEvent> for ReleasingContent {}

    impl Drop for ReleasingContent {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    impl Render for ReleasingContent {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            Button::new("releasing-content")
                .child("Close")
                .debug_selector(|| DIALOG_A.into())
                .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
        }
    }

    /// A window whose root is a [`Layer`], the way an application mounts one, so that the dialogs
    /// have a window layer to be opened on and dismissed from.
    fn harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(style::theme::install);
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| EmptyView)));
        cx.run_until_parked();

        cx
    }

    /// Lets the dialogs' own transitions finish and repaints, so that a dialog just opened is past
    /// its first frames rather than still appearing, and one just dismissed is gone rather than
    /// still fading out.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Opens a dialog whose content is a single button counting the clicks that reach it, which is
    /// how a test tells which of several stacked dialogs the mouse reached.
    fn open_button_dialog(
        cx: &mut VisualTestContext,
        selector: &'static str,
        clicks: Rc<Cell<usize>>,
    ) {
        cx.update(|window, cx| {
            let content = cx.new(|_| CountingButton {
                selector,
                clicks,
                wide: false,
            });

            window.dialog(cx, content);
        });
    }

    /// Opens a dialog whose button is large enough to occupy the middle of the window, which is
    /// where an alert draws its own button as well.
    fn open_wide_button_dialog(
        cx: &mut VisualTestContext,
        selector: &'static str,
        clicks: Rc<Cell<usize>>,
    ) {
        cx.update(|window, cx| {
            let content = cx.new(|_| CountingButton {
                selector,
                clicks,
                wide: true,
            });

            window.dialog(cx, content);
        });
    }

    /// Opens a dialog whose content asks the window for keyed state on every frame, recording the
    /// entity it is handed on each of them.
    fn open_keyed_state_dialog(cx: &mut VisualTestContext, recorded: Rc<RefCell<Vec<EntityId>>>) {
        cx.update(|window, cx| {
            let content = cx.new(|_| KeyedStateProbe { recorded });

            window.dialog(cx, content);
        });
    }

    /// Opens a dialog whose content closes it by emitting, counting the clicks that reach its
    /// button, and hands the content back so that a test can make it emit again whenever it likes.
    fn open_dismissing_dialog(
        cx: &mut VisualTestContext,
        selector: &'static str,
        clicks: Rc<Cell<usize>>,
    ) -> Entity<DismissingContent> {
        cx.update(|window, cx| {
            let content = cx.new(|_| DismissingContent { selector, clicks });

            window.dialog(cx, content.clone());

            content
        })
    }

    /// Opens an alert with no cancel button, counting what its ok button was clicked with.
    fn open_ok_alert(cx: &mut VisualTestContext, ok: Rc<Cell<usize>>) {
        cx.update(|window, cx| {
            window.alert(cx, move |alert, _, _| {
                alert
                    .ok("Saved", "The file has been written.")
                    .ok_action(move |_, _, _| ok.set(ok.get() + 1))
            });
        });
    }

    /// Makes a content emit the event its dialog listens for, which is how a content that is not the
    /// test's own control asks to be closed.
    fn close_emitting(cx: &mut VisualTestContext, content: &Entity<DismissingContent>) {
        cx.update(|_, cx| content.update(cx, |_, cx| cx.emit(DismissEvent)));
        cx.run_until_parked();
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the layer should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        cx.run_until_parked();
    }

    /// A dialog opened over another one is drawn over it and takes the mouse from it: both buttons
    /// are centred on the same point, and the click there reaches the one on top.
    /// Each content closes its own dialog and no other: the layer stacks as many as are opened, an
    /// emit goes to the dialog holding that content rather than to the one on top, and repeating it
    /// while that dialog is leaving does not reach the one underneath.
    #[gpui_kit::test]
    fn each_content_should_dismiss_only_its_own_dialog(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let a = Rc::new(Cell::new(0));
        let b = Rc::new(Cell::new(0));

        open_dismissing_dialog(cx, DIALOG_A, a.clone());
        settle(cx);
        let top = open_dismissing_dialog(cx, DIALOG_B, b.clone());
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the dialog the second one was opened over should stay drawn"
        );
        assert!(
            cx.debug_bounds(DIALOG_B).is_some(),
            "the dialog opened over it should be drawn"
        );

        click(cx, DIALOG_A);

        assert_eq!(
            b.get(),
            1,
            "the dialog on top should take a click that lands on both buttons"
        );
        assert_eq!(
            a.get(),
            0,
            "the dialog underneath should not be reached through the one over it"
        );

        // The emit the click made has closed the top dialog, which is still fading out: a second
        // one arriving now has nothing left to close and must not reach the dialog under it.
        close_emitting(cx, &top);

        assert!(
            cx.debug_bounds(DIALOG_B).is_some(),
            "the dialog that is leaving should still be fading rather than gone"
        );
        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the second emit must not have reached the dialog underneath"
        );
        assert_eq!(
            a.get(),
            0,
            "the dialog underneath should not have been reached by a click either"
        );

        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_B).is_none(),
            "the dismissed dialog should leave the layer once its exit transition is over"
        );
        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the dialog the dismissed one was opened over should stay"
        );

        click(cx, DIALOG_A);

        assert_eq!(
            a.get(),
            1,
            "the remaining dialog should take the mouse once the one over it is gone"
        );

        // The click reached the one content a click could reach, and that click was the last
        // dialog's own way out: it is already closing, so there is nothing here to ask of it.
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "closing the last dialog should leave the layer with no dialog"
        );
    }

    /// Every dialog that was opened is a dialog the layer holds and draws, not just the one on top.
    #[gpui_kit::test]
    fn the_layer_should_hold_as_many_dialogs_as_are_opened(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let mut opened = Vec::new();

        for selector in STACK {
            opened.push(open_dismissing_dialog(cx, selector, Rc::new(Cell::new(0))));
            settle(cx);
        }

        for selector in STACK {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "every dialog opened on the layer should still be drawn"
            );
        }

        for (selector, content) in STACK.into_iter().zip(opened).rev() {
            close_emitting(cx, &content);
            settle(cx);

            assert!(
                cx.debug_bounds(selector).is_none(),
                "closing the dialog on top should take it away"
            );
        }
    }

    /// The alert stays over every dialog: a click on a point a dialog's button occupies reaches the
    /// alert drawn over it instead, and the dialogs are still there once the alert has closed.
    ///
    /// There are two dialogs under it, so an alert drawn between them rather than over both would
    /// leave one of them undrawn or unreached, and neither would survive the alert leaving.
    #[gpui_kit::test]
    fn an_alert_should_stay_over_every_dialog(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let dialog_clicks = Rc::new(Cell::new(0));
        let alert_ok = Rc::new(Cell::new(0usize));

        open_wide_button_dialog(cx, DIALOG_A, dialog_clicks.clone());
        settle(cx);
        open_wide_button_dialog(cx, DIALOG_B, Rc::new(Cell::new(0)));
        settle(cx);

        open_ok_alert(cx, alert_ok.clone());
        settle(cx);

        let button = cx
            .debug_bounds(DIALOG_A)
            .expect("the first dialog's button should be drawn");
        let under = cx
            .debug_bounds(DIALOG_B)
            .expect("the second dialog's button should be drawn");
        let ok = cx
            .debug_bounds(ALERT_OK)
            .expect("the alert's ok button should be drawn");

        assert!(
            button.contains(&ok.center()) && under.contains(&ok.center()),
            "the alert's ok button should sit on a point both dialogs' buttons occupy, or the click \
             below would prove nothing about the alert staying over them"
        );

        cx.simulate_click(ok.center(), Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            alert_ok.get(),
            1,
            "the alert should take a click that lands on the dialogs' buttons too"
        );
        assert_eq!(
            dialog_clicks.get(),
            0,
            "the dialog's button should not be reached through the alert over it"
        );

        // The alert's own ok button, which is how an alert closes itself: it emits the event its
        // dialog listens for, and the alert is what the event closes.
        click(cx, ALERT_OK);
        settle(cx);

        assert!(
            cx.debug_bounds(ALERT).is_none(),
            "a closed alert should leave the layer once its exit transition is over"
        );
        assert!(
            cx.debug_bounds(DIALOG_A).is_some() && cx.debug_bounds(DIALOG_B).is_some(),
            "both dialogs should still be drawn once the alert over them is gone"
        );
    }

    /// There is one alert at a time: opening another one replaces the one showing rather than
    /// stacking over it, so what the user answers is what the layer is holding.
    #[gpui_kit::test]
    fn a_second_alert_should_replace_the_first(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let first_ok = Rc::new(Cell::new(0));
        let first_cancel = Rc::new(Cell::new(0));
        let second_ok = Rc::new(Cell::new(0));

        cx.update(|window, cx| {
            let (ok, cancel) = (first_ok.clone(), first_cancel.clone());

            window.alert(cx, move |alert, _, _| {
                alert
                    .ok_cancel("Discard changes", "Your unsaved changes will be lost.")
                    .ok_action(move |_, _, _| ok.set(ok.get() + 1))
                    .cancel_action(move |_, _| cancel.set(cancel.get() + 1))
            });
        });
        settle(cx);

        assert!(
            cx.debug_bounds(ALERT_CANCEL).is_some(),
            "the first alert should be drawn with the cancel button it was opened with"
        );

        open_ok_alert(cx, second_ok.clone());
        settle(cx);

        assert!(
            cx.debug_bounds(ALERT).is_some(),
            "the second alert should be drawn"
        );
        assert!(
            cx.debug_bounds(ALERT_CANCEL).is_none(),
            "the alert that was replaced had the only cancel button, so it must be off the layer"
        );

        click(cx, ALERT_OK);

        assert_eq!(
            second_ok.get(),
            1,
            "the ok button drawn should be the second alert's"
        );
        assert_eq!(
            first_ok.get(),
            0,
            "the alert that was replaced must not have taken the click"
        );
        assert_eq!(
            first_cancel.get(),
            0,
            "the alert that was replaced must not act at all"
        );

        settle(cx);

        assert!(
            cx.debug_bounds(ALERT).is_none(),
            "the ok button should have taken the second alert off the layer"
        );
    }

    /// A dialog is modal, so it takes the caret out of whatever the dialog under it was holding:
    /// the field stays drawn, but it is no longer where typing goes.
    #[gpui_kit::test]
    fn a_dialog_should_take_the_caret_out_of_what_the_dialog_under_it_was_holding(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx);

        let input = cx.update(|window, cx| {
            cx.new(|cx| InputStateBuilder::new(FOCUS_PROBE).build(window, cx))
        });
        let focus = cx.update(|_, cx| input.read(cx).engine_focus_handle(cx));

        cx.update(|window, cx| {
            let content = cx.new(|_| InputContent {
                input: input.clone(),
            });

            window.dialog(cx, content);
        });
        // The dialog's first frame drops the caret, which nothing held yet.
        settle(cx);

        cx.update(|window, cx| window.focus(&focus, cx));
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_some(),
                "the field in the dialog should hold the caret"
            )
        });
        settle(cx);
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_some(),
                "the field should still hold the caret on the frames that follow: the dialog drops \
                 the caret once, on the frame it is shown, not on every frame it draws"
            )
        });

        open_button_dialog(cx, DIALOG_B, Rc::new(Cell::new(0)));
        settle(cx);

        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_none(),
                "the dialog opened over the field should have taken the caret out of it"
            )
        });
    }

    /// The caret a dialog drops is dropped before its content is built, so content that takes the
    /// caret — a field a form opens on — is what holds it. A blur that came after the content would
    /// take the caret straight back out of it.
    #[gpui_kit::test]
    fn a_dialog_should_let_its_content_take_the_caret_back(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let focus = cx.update(|_, cx| cx.focus_handle());
        let focused = Rc::new(Cell::new(false));

        cx.update(|window, cx| {
            let content = cx.new(|_| FocusTakingContent {
                focus: focus.clone(),
                focused: focused.clone(),
            });

            window.dialog(cx, content);
        });
        settle(cx);

        assert!(
            focused.get(),
            "the dialog's content should have been built and should have taken the caret"
        );
        cx.update(|window, _| {
            assert!(
                focus.is_focused(window),
                "the window should hold the caret the dialog's own content asked for: the dialog \
                 drops the caret of what was behind it, and then the content is what is drawn"
            )
        });
    }

    /// A dialog opened from inside another dialog's content — a form opened by the page that asked
    /// for it — is still drawn, without anything refreshing the window. The layer pushes it at the
    /// end of the effect cycle, when the frame is over and the notification it makes asks for the
    /// next one; a push made while the frame was drawing would mark nothing dirty, and the dialog
    /// would sit on the layer with no frame ever drawing it.
    #[gpui_kit::test]
    fn a_dialog_opened_while_another_is_drawn_should_still_be_drawn(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let opened = Rc::new(Cell::new(false));

        cx.update(|window, cx| {
            let content = cx.new(|_| NestedDialogOpener {
                opened: opened.clone(),
            });

            window.dialog(cx, content);
        });
        // No refresh: the only frame that can draw the nested dialog is the one the deferred push
        // asked for, which is the whole of what this pins.
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the dialog the nested one was opened from should be drawn"
        );
        assert!(
            cx.debug_bounds(DIALOG_B).is_some(),
            "the dialog opened while another one was drawing should be drawn"
        );
    }

    /// A dialog created and dismissed before its first frame draws nothing at all: it was never
    /// shown, so there is nothing to fade out, and drawing it anyway would leave an invisible
    /// dialog in the tree. Opacity is applied at paint while the hitboxes are inserted at prepaint,
    /// so an invisible dialog still occludes the pointer — for as long as it is drawn, which is
    /// however long the exit transition it is waiting on takes, measured against a clock a test
    /// never moves.
    #[gpui_kit::test]
    fn a_dialog_dismissed_before_it_is_shown_should_draw_nothing(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let content = cx.update(|_, cx| {
            cx.new(|_| CountingButton {
                selector: DIALOG_A,
                clicks: Rc::new(Cell::new(0)),
                wide: false,
            })
        });

        cx.update(|window, cx| {
            window.dialog(cx, content.clone());
            // Deferred as well, and deferred after the push rather than before it: the push is what
            // puts this dialog on the layer, so a close made here and now would reach the dialog
            // that was on top before it, and this one would never be closed at all.
            window.defer(cx, move |_, cx| {
                content.update(cx, |_, cx| cx.emit(DismissEvent))
            });
        });
        // No refresh: the frames that run are the ones the deferred push asked for. The clock moves
        // so that a dialog that were drawn is given every chance to finish leaving, rather than
        // being pinned at the start of its exit transition by a clock that stands still.
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "a dialog that was never shown has nothing to fade out and must not be drawn at all"
        );
    }

    /// A dialog that is leaving takes nothing of its own content: it is still drawn while it fades
    /// out, but what it draws no longer acts, so the second click of a double-click cannot run the
    /// action the first one ran a second time.
    #[gpui_kit::test]
    fn a_dismissed_dialog_should_not_act_again_while_it_is_fading(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let clicks = Rc::new(Cell::new(0));

        open_dismissing_dialog(cx, DIALOG_A, clicks.clone());
        settle(cx);

        click(cx, DIALOG_A);

        assert_eq!(
            clicks.get(),
            1,
            "the click should have reached the button the first time"
        );
        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the dismissed dialog should still be drawn while it fades out"
        );

        // The second click of the double-click, on the same point, while the dialog is still there
        // and still fading: the clock is not moved and the window is not refreshed in between, so
        // the guard has to be installed by the frame the dismissal itself asked for.
        click(cx, DIALOG_A);

        assert_eq!(
            clicks.get(),
            1,
            "a dialog that is leaving must not run its content's action again"
        );

        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "the dialog should be gone once its exit transition is over"
        );
    }

    /// Two dialogs keep their own keyed element state: GPUI scopes that state by the view drawing
    /// it, and every dialog's content is its own view entity, so stacked dialogs never share it.
    /// That is why the layer can stack them as they are, with no id wrapper around them.
    ///
    /// What is asserted is that the first dialog draws *again*, with the same state, while the
    /// second is on the layer: a single-slot layer would leave the first one undrawn, and a layer
    /// that gave both dialogs one scope would leave the first one reading what the second was
    /// handed.
    #[gpui_kit::test]
    fn two_dialogs_should_keep_their_own_keyed_element_state(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let a = Rc::new(RefCell::new(Vec::new()));
        let b = Rc::new(RefCell::new(Vec::new()));

        open_keyed_state_dialog(cx, a.clone());
        settle(cx);

        let first = a.borrow().last().copied().expect(
            "the dialog opened first should have drawn the keyed state on the frame it opened on",
        );
        // What it records from here on is what it draws while the second dialog is on the layer.
        a.borrow_mut().clear();

        open_keyed_state_dialog(cx, b.clone());
        settle(cx);

        let a = a.borrow();
        let b = b.borrow();
        assert!(
            !a.is_empty(),
            "the dialog opened first should still be drawn, and still be handed keyed state, once \
             another dialog is opened over it"
        );
        assert!(
            a.iter().all(|id| *id == first),
            "the dialog opened first should be handed the same keyed state on every frame it \
             draws, not the state of the dialog opened over it"
        );
        let second = b
            .last()
            .expect("the dialog opened over it should have drawn the keyed state");
        assert_ne!(
            first, *second,
            "the two dialogs should be handed a state entity each, so a dialog never reads the \
             keyed state of the one it was opened over"
        );
    }

    /// Two stacked dialogs are two content entities, and each of them draws the entity the layer
    /// was handed rather than one made for the frame: that is what makes the state a content owns
    /// survive the frames it is drawn on, which is the whole of what a content entity buys.
    #[gpui_kit::test]
    fn two_dialogs_should_hold_their_own_content_entity(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let first_drawn = Rc::new(RefCell::new(Vec::new()));
        let second_drawn = Rc::new(RefCell::new(Vec::new()));

        let first = cx.update(|_, cx| {
            cx.new(|_| IdentityProbe {
                drawn: first_drawn.clone(),
            })
        });
        cx.update(|window, cx| window.dialog(cx, first.clone()));
        settle(cx);

        let second = cx.update(|_, cx| {
            cx.new(|_| IdentityProbe {
                drawn: second_drawn.clone(),
            })
        });
        cx.update(|window, cx| window.dialog(cx, second.clone()));
        settle(cx);

        assert_ne!(
            first.entity_id(),
            second.entity_id(),
            "the two dialogs should hold a content entity each"
        );

        let first_drawn = first_drawn.borrow();
        let second_drawn = second_drawn.borrow();

        assert!(
            !first_drawn.is_empty() && !second_drawn.is_empty(),
            "both dialogs should have drawn their content"
        );
        assert!(
            first_drawn.iter().all(|id| *id == first.entity_id()),
            "the dialog opened first should draw the entity it was handed on every frame, not one \
             made for the frame"
        );
        assert!(
            second_drawn.iter().all(|id| *id == second.entity_id()),
            "the dialog opened second should draw the entity it was handed on every frame, not the \
             one the dialog under it was handed"
        );
    }

    /// The dialog that finished leaving is the one removed, even when another dialog has been
    /// opened over it in the meantime.
    #[gpui_kit::test]
    fn a_dialog_that_leaves_under_another_one_should_be_the_one_removed(cx: &mut TestAppContext) {
        let cx = harness(cx);

        let first = open_dismissing_dialog(cx, DIALOG_A, Rc::new(Cell::new(0)));
        settle(cx);

        close_emitting(cx, &first);
        open_dismissing_dialog(cx, DIALOG_B, Rc::new(Cell::new(0)));
        settle(cx);
        // The removal happens while that frame is drawn, and a frame is built from the stack as it
        // was when the frame started: the second one is drawn from the stack the removal left
        // behind, and is what tells the two dialogs apart.
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_B).is_some(),
            "the dialog opened over the one that is leaving should stay"
        );
        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "the dialog that finished leaving should be the one removed, not the one over it"
        );
    }

    /// A content that asks to be closed in the same effect cycle that opened it is still heard: the
    /// dialog — and so the subscription — is made where `window.dialog` is called rather than in the
    /// deferred push, and a subscription's activation effect is queued where `subscribe` is called,
    /// ahead of anything the caller defers or emits afterwards.
    ///
    /// The shape this pins is a content opened and then told to close in the same turn: a form whose
    /// opening is immediately refused, a task that finishes before the first frame. It is the one
    /// case where the content's own signal could be dispatched to nobody without anyone noticing.
    #[gpui_kit::test]
    fn a_content_that_emits_in_the_cycle_that_opened_it_should_still_close_its_dialog(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx);
        let emitted = Rc::new(Cell::new(0));

        cx.update(|window, cx| {
            let content = cx.new(|_| EmittingContent {
                selector: DIALOG_A,
                emitted,
            });

            window.dialog(cx, content.clone());
            content.update(cx, |_, cx| cx.emit(DismissEvent));
        });
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "an emit made in the cycle that opened the dialog should close it: the subscription has \
             to be live by then, or the content's own way out is silently dropped"
        );
    }

    /// An alert is built when it is opened, not while the dialog that draws it renders: building one
    /// that has an input field makes that field's entity, and an entity made in the middle of a
    /// frame is the re-entrancy this refactor set out to remove.
    ///
    /// What is asserted is that the builder has already run by the time the call that opened the
    /// alert returns — before any frame is drawn, so nowhere near `Render`.
    #[gpui_kit::test]
    fn an_alert_should_be_built_when_it_is_opened_rather_than_while_it_draws(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx);
        let built = Rc::new(Cell::new(false));
        let input = Rc::new(Cell::new(false));

        cx.update(|window, cx| {
            let (built, input) = (built.clone(), input.clone());

            window.alert(cx, move |alert, window, cx| {
                built.set(true);
                // The shape a real alert with a field has: the builder makes an entity of its own.
                if cx
                    .new(|cx| InputStateBuilder::new("alert-input").build(window, cx))
                    .read(cx)
                    .value(cx)
                    .is_empty()
                {
                    input.set(true);
                }

                alert.ok("Saved", "The file has been written.")
            });
        });

        assert!(
            built.get() && input.get(),
            "the alert builder should have run by the time the call that opened it returned, which is \
             what keeps the field it makes out of a frame"
        );
    }

    /// A content that emits the event its dialog subscribes to closes that dialog, which is the
    /// content saying it is done from its own context rather than through the window.
    #[gpui_kit::test]
    fn a_content_that_emits_a_dismiss_event_should_close_its_dialog(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let clicks = Rc::new(Cell::new(0));

        // The handle is not kept: the dialog holds the only strong reference from here, which is
        // what the button being drawn at all shows.
        open_dismissing_dialog(cx, DIALOG_A, clicks.clone());
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the dialog whose content was handed to the layer should be drawn"
        );

        click(cx, DIALOG_A);

        assert_eq!(
            clicks.get(),
            1,
            "the click should have reached the content's own button"
        );

        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "the dialog whose content emitted should have left the layer once its exit transition \
             was over"
        );
    }

    /// A content that emits closes its own dialog wherever that dialog sits on the layer: the
    /// signal goes to the dialog that holds the content, not to the one on top, so a dialog under
    /// two others can still close itself.
    #[gpui_kit::test]
    fn a_content_that_emits_should_close_only_its_own_dialog(cx: &mut TestAppContext) {
        let cx = harness(cx);

        open_button_dialog(cx, DIALOG_A, Rc::new(Cell::new(0)));
        settle(cx);
        let middle = open_dismissing_dialog(cx, DIALOG_B, Rc::new(Cell::new(0)));
        settle(cx);
        open_button_dialog(cx, STACK[0], Rc::new(Cell::new(0)));
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_some()
                && cx.debug_bounds(DIALOG_B).is_some()
                && cx.debug_bounds(STACK[0]).is_some(),
            "all three dialogs should be drawn before the middle one closes itself"
        );

        cx.update(|_, cx| middle.update(cx, |_, cx| cx.emit(DismissEvent)));
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_B).is_none(),
            "the dialog whose content emitted should have left the layer"
        );
        assert!(
            cx.debug_bounds(STACK[0]).is_some(),
            "the dialog over the one that emitted should stay: a content closes its own dialog, \
             not the one on top"
        );
        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the dialog under the one that emitted should stay"
        );
    }

    /// A content that emits again while its dialog is already playing its exit transition does
    /// nothing more: the dialog is closed, so the second signal has nothing left to close and
    /// cannot reach the dialog underneath either.
    #[gpui_kit::test]
    fn a_content_that_emits_while_its_dialog_is_fading_should_not_close_it_twice(
        cx: &mut TestAppContext,
    ) {
        let cx = harness(cx);
        let emitted = Rc::new(Cell::new(0));

        open_button_dialog(cx, DIALOG_A, Rc::new(Cell::new(0)));
        settle(cx);
        let content = open_dismissing_dialog(cx, DIALOG_B, emitted.clone());
        settle(cx);

        click(cx, DIALOG_B);

        assert_eq!(
            emitted.get(),
            1,
            "the click should have reached the content once"
        );
        assert!(
            cx.debug_bounds(DIALOG_B).is_some(),
            "the dialog should still be drawn while it fades out"
        );

        // The clock is not moved and the window is not refreshed in between, so this emit arrives
        // on the frame that is drawing the fade the first one started.
        close_emitting(cx, &content);
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the second emit must not have reached the dialog under the one that is leaving"
        );
        assert!(
            cx.debug_bounds(DIALOG_B).is_some(),
            "the dialog that is leaving should still be fading rather than gone"
        );

        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_B).is_none(),
            "the dialog should be gone once its exit transition is over"
        );
        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the dialog underneath should have been left alone throughout"
        );
    }

    /// The content entity is released once its dialog has left the layer: the layer holds the last
    /// strong reference to it, so whatever state the content owned is dropped with it and no caller
    /// has any cleanup to do.
    ///
    /// The dialog is closed the one way a dialog closes: by its content, from the content's own
    /// button.
    #[gpui_kit::test]
    fn a_dialog_should_release_its_content_when_it_closes(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let dropped = Rc::new(Cell::new(false));

        cx.update(|window, cx| {
            let content = cx.new(|_| ReleasingContent {
                dropped: dropped.clone(),
            });

            window.dialog(cx, content);
        });
        settle(cx);

        assert!(
            !dropped.get(),
            "the content should be alive while the dialog drawing it is open"
        );

        click(cx, DIALOG_A);
        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "the closed dialog should have left the layer"
        );
        assert!(
            dropped.get(),
            "the content should have been released once its dialog left the layer"
        );
    }

    /// The content outlives the close and goes with the dialog: it is still alive — and still the
    /// thing being drawn — while the dialog fades out, and is released only once the dialog has
    /// left. That is the whole of the cleanup contract: nothing outside the content has to know
    /// when it ends.
    #[gpui_kit::test]
    fn a_dialog_should_release_its_content_only_after_its_exit_transition(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let dropped = Rc::new(Cell::new(false));

        // The handle is not kept: a test holding one would be a second strong reference, and the
        // content would outlive the dialog for as long as the test does. What the dialog holds is
        // what is being watched here.
        cx.update(|window, cx| {
            let content = cx.new(|_| ReleasingContent {
                dropped: dropped.clone(),
            });

            window.dialog(cx, content);
        });
        settle(cx);

        // Closed by its own content, which emits from the button's click handler: the content is
        // what ends the dialog, and the layer is told rather than asked.
        click(cx, DIALOG_A);

        assert!(
            cx.debug_bounds(DIALOG_A).is_some(),
            "the closed dialog should still be drawn while it fades out"
        );
        assert!(
            !dropped.get(),
            "the content should outlive the close: the dialog that is fading out still draws it"
        );

        settle(cx);

        assert!(
            cx.debug_bounds(DIALOG_A).is_none(),
            "the dialog should be gone once its exit transition is over"
        );
        assert!(
            dropped.get(),
            "the content should have been released once its dialog left the layer"
        );
    }

    /// The dialog holds a strong reference to the content it was handed, so the caller's own handle
    /// stays readable — and the state behind it stays the state being drawn — for as long as the
    /// dialog is on the layer.
    #[gpui_kit::test]
    fn a_dialog_should_keep_its_content_alive_while_it_is_open(cx: &mut TestAppContext) {
        let cx = harness(cx);
        let clicks = Rc::new(Cell::new(0));
        let content = cx.update(|_, cx| {
            cx.new(|_| CountingButton {
                selector: DIALOG_A,
                clicks: clicks.clone(),
                wide: false,
            })
        });

        cx.update(|window, cx| window.dialog(cx, content.clone()));
        settle(cx);

        cx.update(|_, cx| {
            assert_eq!(
                content.read(cx).selector,
                DIALOG_A,
                "the content entity should still be readable while the dialog drawing it is open"
            );
        });

        click(cx, DIALOG_A);

        assert_eq!(
            clicks.get(),
            1,
            "the content the test holds a handle of should be the one the dialog draws"
        );
    }
}

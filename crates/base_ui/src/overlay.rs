//! A panel that belongs to a trigger and is drawn over everything else.
//!
//! Every popup in this crate is the same control underneath: something the user
//! operates, a panel that opens beside it, and a set of rules about when that panel
//! is on screen. Those rules — where the panel goes, how it fades in and out, and
//! that a press anywhere else dismisses it — are what this module owns, so that the
//! components built on it differ only in what they are made of: a [`crate::dropdown`]
//! in its entries, a [`crate::select`] in its selection, a tooltip in its text.
//!
//! What it deliberately does not own is *when* the panel opens. Opening is a
//! property of the control — a click, a hover, a caret arriving — so the trigger is
//! built by the caller, which is handed the shared state to change.
//!
//! [`Overlay`] is not named `Anchored` because [`gpui_kit::Anchored`] is a different
//! element — one that pins a child to a corner of its parent — and the two would
//! collide in every file that draws with the prelude.

use gpui_kit::{base::*, prelude::FluentBuilder, *};

use crate::animation::{ANIMATION_TICK, popup_should_render};

/// What a panel is handed when it is built.
///
/// An open panel is not a still picture of a control: it follows the trigger it
/// belongs to as the window moves under it, it fades, and it closes when the user
/// asks it to. All three are here, so a panel that has to react to one of them —
/// an entry that closes the panel it was chosen from, a layout as wide as the
/// trigger — can, without reaching back into the control that opened it.
pub struct PanelContext {
    /// The state that says whether the panel is open. Writing `false` to it closes
    /// the panel.
    pub open_state: Entity<bool>,
    /// Where the trigger is, which is what the panel is placed against.
    pub trigger_bounds: Bounds<Pixels>,
    /// How far the panel is through opening or closing.
    pub sample: PresenceSample,
}

/// Builds the trigger: the thing the user operates to open the panel.
///
/// It is handed the open state so that a trigger can look pressed while the panel is
/// open and write to it when the user asks for one or the other; a trigger that only
/// reacts to hovering, like a tooltip's, can ignore it.
///
/// It is called once per rendering, so it may consume the elements it was given.
pub type TriggerBuilder = Box<dyn FnOnce(Entity<bool>, &mut Window, &mut App) -> AnyElement>;

/// Builds the panel's content, once, at the moment the panel becomes something to
/// draw.
///
/// It is not called at all while the panel is closed, and it is handed how far the
/// panel is through opening so that content which follows the fade can.
pub type PanelBuilder = Box<dyn FnOnce(PanelContext, &mut Window, &mut App) -> AnyElement>;

/// A trigger with a panel that opens beside it.
///
/// The panel is placed on the requested side of the trigger, flipping to the other
/// side when it would leave the window; it fades in and out over
/// [`ANIMATION_TICK`], and it closes when the user presses outside it. A panel that
/// is closed is not in the tree at all: a popup occludes the pointer, so one left
/// there while invisible would swallow clicks meant for what is behind it.
///
/// The panel is laid out as its own box — the caller decides its width, height,
/// background and role, and draws whatever it likes inside. It is not wrapped in a
/// menu, a card or any other shape.
///
/// Whether it is open is the caller's to keep: a control whose trigger has to look
/// pressed, or whose panel closes on something other than a press outside it, needs
/// the same state. What this owns is what happens to the panel because of it.
#[derive(IntoElement)]
pub struct Overlay {
    id: ElementId,
    open_state: Entity<bool>,
    trigger: TriggerBuilder,
    panel: PanelBuilder,
    style: StyleRefinement,
    placement: Placement,
    align: Align,
    priority: usize,
}

impl Overlay {
    pub fn new(
        id: impl Into<ElementId>,
        open_state: Entity<bool>,
        trigger: impl FnOnce(Entity<bool>, &mut Window, &mut App) -> AnyElement + 'static,
        panel: impl FnOnce(PanelContext, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            open_state,
            trigger: Box::new(trigger),
            panel: Box::new(panel),
            style: StyleRefinement::default(),
            placement: Placement::Bottom,
            align: Align::Center,
            priority: POPUP_PRIORITY,
        }
    }

    /// Sets the side of the trigger the panel prefers, which it keeps to unless the
    /// panel would not fit there.
    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// Sets how the panel lines up with the trigger along that side.
    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    /// Sets how far above other popups this one is drawn.
    ///
    /// A panel that was opened from another panel is drawn over it, so a tooltip
    /// inside an open menu is not buried under the menu it belongs to.
    pub fn priority(mut self, priority: usize) -> Self {
        self.priority = priority;
        self
    }
}

impl Styled for Overlay {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Overlay {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            open_state,
            trigger,
            panel,
            style,
            placement,
            align,
            priority,
        } = self;

        let opened = *open_state.read(cx);

        let trigger_bounds_state =
            window.use_keyed_state((id.clone(), "trigger-bounds"), cx, |_, _| Bounds::default());
        let trigger_bounds = *trigger_bounds_state.read(cx);

        // The trigger is measured as it is drawn, so the panel follows it when the
        // window it sits in is moved or resized while the panel is open.
        let trigger = div()
            .id((id.clone(), "trigger"))
            .on_prepaint(move |bounds, _, cx| *trigger_bounds_state.as_mut(cx) = bounds)
            .child(trigger(open_state.clone(), window, cx));

        let sample = Presence::new((id.clone(), "presence-notice"), opened)
            .transition(Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut))
            .sample(window, cx);

        div()
            .id(id)
            .child(trigger)
            // The panel is built only when it is something to draw, and it is built on
            // the frame it is drawn on rather than earlier: a panel handed an element
            // that can only be drawn once would have nothing left to draw on the next
            // frame of its fade.
            .when(popup_should_render(opened, sample), |this| {
                this.child(
                    deferred(
                        Positioner::side(trigger_bounds)
                            .placement(placement)
                            .align(align)
                            .margin(px(4.))
                            .offset(px(4.))
                            .occlude()
                            .child(
                                div()
                                    .top(px(-4.0) * (1.0 - sample.progress))
                                    .opacity(sample.progress)
                                    .child(panel(
                                        PanelContext {
                                            open_state: open_state.clone(),
                                            trigger_bounds,
                                            sample,
                                        },
                                        window,
                                        cx,
                                    ))
                                    .on_mouse_down_out(move |_, _, cx| open_state.write(cx, false)),
                            ),
                    )
                    .with_priority(priority),
                )
            })
            .refine_style(&style)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui_kit::{
        AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, Modifiers,
        MouseButton, ParentElement as _, Render, StatefulInteractiveElement, Styled as _,
        TestAppContext, VisualTestContext, Window, div, point, px, size,
    };

    use super::Overlay;
    use crate::{button::Button, interactive::InteractiveVariantExt, size::UnifiedSizable, style};

    /// A trigger with a panel, above a button an *open* panel would cover.
    struct Harness {
        clicks: Rc<Cell<usize>>,
        open_state: Entity<bool>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();

            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .child(Overlay::new(
                    "test-overlay",
                    self.open_state.clone(),
                    |open_state, _, _| {
                        Button::new("test-overlay-trigger")
                            .tonal()
                            .small()
                            .debug_selector(|| "TEST_OVERLAY_TRIGGER".into())
                            .child("trigger")
                            .on_click(move |_, _, cx| open_state.write(cx, true))
                            .into_any_element()
                    },
                    |_, _, _| {
                        div()
                            .debug_selector(|| "TEST_OVERLAY_PANEL".into())
                            .child("panel")
                            .into_any_element()
                    },
                ))
                .child(
                    div().debug_selector(|| "TEST_OVERLAY_TARGET".into()).child(
                        Button::new("test-overlay-target")
                            .tonal()
                            .small()
                            .child("target")
                            .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                    ),
                )
        }
    }

    /// A harness sized and drawn, with the button behind it that counts clicks.
    fn harness(cx: &mut TestAppContext) -> (Rc<Cell<usize>>, &mut VisualTestContext) {
        cx.update(style::theme::install);
        let clicks = Rc::new(Cell::new(0));
        let open_state = cx.update(|cx| cx.new(|_| false));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Harness { clicks, open_state }
        });
        cx.simulate_resize(size(px(400.0), px(400.0)));
        cx.run_until_parked();

        (clicks, cx)
    }

    fn shows(cx: &mut VisualTestContext, selector: &'static str) -> bool {
        cx.debug_bounds(selector).is_some()
    }

    /// A panel that was never opened draws nothing, so a click under its trigger
    /// belongs to whatever is drawn there.
    #[gpui_kit::test]
    fn a_panel_that_was_never_opened_should_not_swallow_a_click_behind_it(cx: &mut TestAppContext) {
        let (clicks, cx) = harness(cx);

        let target = cx
            .debug_bounds("TEST_OVERLAY_TARGET")
            .expect("the harness draws its target");
        cx.simulate_click(target.center(), Modifiers::none());

        assert_eq!(
            clicks.get(),
            1,
            "a panel that has never been opened must not cover what is behind it"
        );
    }

    /// Opening the panel is the trigger's job, not the shell's: the panel appears
    /// only once the state the trigger writes says it should.
    #[gpui_kit::test]
    fn a_panel_should_open_once_its_state_says_it_is_open(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);

        assert!(!shows(cx, "TEST_OVERLAY_PANEL"));
        let trigger = cx
            .debug_bounds("TEST_OVERLAY_TRIGGER")
            .expect("the harness draws its trigger");
        cx.simulate_click(trigger.center(), Modifiers::none());
        cx.run_until_parked();

        assert!(shows(cx, "TEST_OVERLAY_PANEL"));
    }

    /// A press anywhere outside the panel closes it, which is the dismissal every
    /// popup in this crate shares.
    #[gpui_kit::test]
    fn pressing_outside_an_open_panel_should_close_it(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);
        let trigger = cx
            .debug_bounds("TEST_OVERLAY_TRIGGER")
            .expect("the harness draws its trigger");
        cx.simulate_click(trigger.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(shows(cx, "TEST_OVERLAY_PANEL"));

        cx.simulate_mouse_down(
            point(px(2.0), px(398.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();

        assert!(
            !shows(cx, "TEST_ANCHORED_PANEL"),
            "a press outside the panel dismisses it"
        );
    }

    /// The state is the caller's, so a caller can open a panel without a press.
    #[gpui_kit::test]
    fn a_panel_should_open_when_the_caller_opens_it(cx: &mut TestAppContext) {
        cx.update(style::theme::install);
        let open_state = cx.update(|cx| cx.new(|_| false));
        let draw_state = open_state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| Harness {
            clicks: Rc::new(Cell::new(0)),
            open_state: draw_state,
        });
        cx.simulate_resize(size(px(400.0), px(400.0)));
        cx.run_until_parked();
        assert!(!shows(cx, "TEST_OVERLAY_PANEL"));

        cx.update(|_, cx| open_state.write(cx, true));
        cx.run_until_parked();

        assert!(shows(cx, "TEST_OVERLAY_PANEL"));
    }
}

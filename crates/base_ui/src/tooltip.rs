use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, Window,
    base::{POPUP_PRIORITY, Placement},
    div,
};

use crate::{overlay::Overlay, style::theme::ThemeExt};

pub trait TooltipExt {
    fn with_tooltip(self, content: impl IntoElement) -> Self;
}

#[derive(IntoElement)]
pub struct Tooltip {
    id: ElementId,
    trigger: AnyElement,
    content: Option<AnyElement>,
    placement: Placement,
}

impl Tooltip {
    pub fn new(id: impl Into<ElementId>, trigger: impl IntoElement) -> Self {
        Self {
            id: id.into(),
            trigger: trigger.into_any_element(),
            content: None,
            placement: Placement::Bottom,
        }
    }

    pub fn content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }
}

impl RenderOnce for Tooltip {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            trigger,
            content,
            placement,
        } = self;

        // A tooltip without anything to say is not a control: it draws what it was
        // given to point at and nothing else.
        let Some(content) = content else {
            return trigger;
        };

        let theme = cx.theme();
        let background = theme.components.tooltip.background;
        let foreground = theme.components.tooltip.foreground;

        let content_id = id.clone();
        // Hovering is what opens a tooltip, so its state belongs here rather than to a
        // caller: nothing outside the tooltip decides when it is on screen.
        let open_state = window.use_keyed_state((id.clone(), "open"), cx, |_, _| false);
        let trigger_id = id.clone();
        // An element can be drawn once, and the panel is built on every frame of its
        // fade, so the content is handed to the frame that draws it and the frames
        // after it are given nothing to draw.
        let content = Rc::new(RefCell::new(Some(content)));

        Overlay::new(
            id,
            open_state,
            move |open_state, _, _| {
                div()
                    .id((trigger_id.clone(), "trigger"))
                    .on_hover(move |hovered, _, cx| open_state.write(cx, *hovered))
                    .child(trigger)
                    .into_any_element()
            },
            move |_, _, _| {
                div()
                    .debug_selector(|| "BASE_UI_TOOLTIP".into())
                    .child(
                        gpui_kit::base::Tooltip::new((content_id.clone(), "tooltip-content"))
                            .rounded_sm()
                            .bg(background.hsla)
                            .text_color(foreground.hsla)
                            .flex()
                            .items_center()
                            .justify_center()
                            .p_1()
                            .text_xs()
                            .children(content.borrow_mut().take()),
                    )
                    .into_any_element()
            },
        )
        .placement(placement)
        // A tooltip points at something inside a panel as often as it points at a
        // control of its own, and it must not be buried under the panel it explains.
        .priority(POPUP_PRIORITY + 1)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::{
        AppContext, Context, InteractiveElement, IntoElement, Modifiers, ParentElement, Render,
        Styled, TestAppContext, VisualTestContext, Window, div, point, px, size,
    };

    use crate::{animation::ANIMATION_TICK, layer::Layer, style, tooltip::Tooltip};

    const TOOLTIP: &str = "BASE_UI_TOOLTIP";
    const FREE_TRIGGER: &str = "TEST_FREE_TRIGGER";
    const OCCLUDING_TRIGGER: &str = "TEST_OCCLUDING_TRIGGER";
    const WRAPPED_TRIGGER: &str = "TEST_WRAPPED_TRIGGER";

    /// Three ways to trigger one tooltip, so the shapes can be compared.
    struct Harness;

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_row()
                .gap_4()
                .child(
                    Tooltip::new(
                        "free",
                        div().debug_selector(|| FREE_TRIGGER.into()).w_16().h_8(),
                    )
                    .content("free"),
                )
                .child(
                    Tooltip::new(
                        "occluding",
                        div()
                            .occlude()
                            .debug_selector(|| OCCLUDING_TRIGGER.into())
                            .w_16()
                            .h_8(),
                    )
                    .content("occluding"),
                )
                // The shape a trigger that has to occlude is meant to use: the
                // tooltip sits inside the occluding element, so the trigger itself
                // does not stop the hover from reaching the tooltip.
                .child(
                    div().occlude().child(
                        Tooltip::new(
                            "wrapped",
                            div().debug_selector(|| WRAPPED_TRIGGER.into()).w_16().h_8(),
                        )
                        .content("wrapped"),
                    ),
                )
        }
    }

    fn harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            style::theme::install(cx);
        });
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Harness)));
        cx.simulate_resize(size(px(480.0), px(160.0)));
        cx.run_until_parked();

        cx
    }

    /// Lets the tooltip's own delay and fade pass.
    fn settle(cx: &mut VisualTestContext) {
        for _ in 0..10 {
            cx.executor()
                .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }
    }

    /// Hovers one trigger and lets the tooltip's own delay and fade pass.
    fn hover(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the harness should draw {selector}"))
            .center();
        cx.simulate_mouse_move(target, None, Modifiers::none());
        cx.run_until_parked();
        settle(cx);
    }

    #[gpui_kit::test]
    fn a_tooltip_should_open_while_its_trigger_is_hovered(cx: &mut TestAppContext) {
        let cx = harness(cx);

        hover(cx, FREE_TRIGGER);
        let panel = cx
            .debug_bounds(TOOLTIP)
            .expect("hovering a trigger should open its tooltip");
        assert!(
            panel.size.width < px(120.0) && panel.size.height < px(60.0),
            "the panel should hug its text, not fill the window: {panel:?}"
        );

        let away = cx
            .debug_bounds(FREE_TRIGGER)
            .expect("the trigger is drawn")
            .center();
        cx.simulate_mouse_move(point(px(10.0), away.y + px(80.0)), None, Modifiers::none());
        cx.run_until_parked();
        settle(cx);

        assert!(
            cx.debug_bounds(TOOLTIP).is_none(),
            "leaving the trigger should close the tooltip"
        );
    }

    /// A trigger that occludes can never open its tooltip: GPUI stops its hover
    /// hit test at the first occluding hitbox, and the trigger is painted after
    /// the tooltip's own hover wrapper, so the wrapper never hears the hover.
    ///
    /// This is the constraint that makes an occluding control wrap its tooltip
    /// instead of occluding itself.
    #[gpui_kit::test]
    fn a_tooltip_should_not_open_from_an_occluding_trigger(cx: &mut TestAppContext) {
        let cx = harness(cx);

        hover(cx, OCCLUDING_TRIGGER);

        assert!(
            cx.debug_bounds(TOOLTIP).is_none(),
            "an occluding trigger blocks the tooltip it was given"
        );
    }

    #[gpui_kit::test]
    fn a_tooltip_should_open_when_the_occlusion_is_on_a_wrapper(cx: &mut TestAppContext) {
        let cx = harness(cx);

        hover(cx, WRAPPED_TRIGGER);

        assert!(
            cx.debug_bounds(TOOLTIP).is_some(),
            "an occluding wrapper keeps both the occlusion and the tooltip"
        );
    }
}

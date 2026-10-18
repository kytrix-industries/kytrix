use std::rc::Rc;

use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, Entity, InteractiveElement, Interactivity, IntoElement,
    ParentElement, Pixels, RenderOnce, Role, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, Window, base::*, div, prelude::FluentBuilder,
};
use smallvec::SmallVec;

use crate::{
    interactive::{Interactive, InteractiveVariantExt},
    menu::{Menu, MenuItem},
    overlay::Overlay,
    size::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle},
    tooltip::TooltipExt,
};

#[derive(IntoElement)]
pub struct Dropdown {
    id: ElementId,
    base: Interactive,
    style: StyleRefinement,
    children: SmallVec<[AnyElement; 2]>,
    menu: Menu,
    on_menu_item_click: Option<Rc<dyn Fn(&ClickEvent, SharedString, &mut Window, &mut App)>>,
    open_state: Option<Entity<bool>>,
    unified_size: UnifiedSize,
    disabled: bool,
    placement: Placement,
    align: Align,
    menu_max_h: Option<Pixels>,
}

impl Dropdown {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        let base = Interactive::new(id.clone());
        let menu = Menu::new((id.clone(), "menu"));

        Self {
            id,
            base,
            style: StyleRefinement::default(),
            children: SmallVec::new(),
            menu,
            on_menu_item_click: None,
            open_state: None,
            unified_size: UnifiedSize::Medium,
            disabled: false,
            placement: Placement::Bottom,
            align: Align::Center,
            menu_max_h: None,
        }
    }

    pub fn menu_item(mut self, menu_item: MenuItem) -> Self {
        self.menu = self.menu.item(menu_item);
        self
    }

    pub fn on_menu_item_click(
        mut self,
        on_menu_item_click: impl Fn(&ClickEvent, SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_menu_item_click = Some(Rc::new(on_menu_item_click));
        self
    }

    pub fn open_state(mut self, open_state: &Entity<bool>) -> Self {
        self.open_state = Some(open_state.clone());
        self
    }

    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn menu_max_h(mut self, menu_max_h: impl Into<Pixels>) -> Self {
        self.menu_max_h = Some(menu_max_h.into());
        self
    }
}

impl Styled for Dropdown {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for Dropdown {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl InteractiveElement for Dropdown {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Dropdown {}

impl InteractiveVariantExt for Dropdown {
    fn filled(mut self) -> Self {
        self.base = self.base.filled();
        self
    }

    fn tonal(mut self) -> Self {
        self.base = self.base.tonal();
        self
    }

    fn outline(mut self) -> Self {
        self.base = self.base.outline();
        self
    }

    fn ghost(mut self) -> Self {
        self.base = self.base.ghost();
        self
    }
}

impl Disableable for Dropdown {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl UnifiedSizable for Dropdown {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = size;
        self
    }
}

impl TooltipExt for Dropdown {
    fn with_tooltip(mut self, content: impl IntoElement) -> Self {
        self.base = self.base.with_tooltip(content);
        self
    }
}

impl RenderOnce for Dropdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            base,
            style,
            children,
            menu,
            on_menu_item_click,
            open_state,
            unified_size,
            disabled,
            placement,
            align,
            menu_max_h,
        } = self;

        let open_state =
            open_state.unwrap_or(window.use_keyed_state((id.clone(), "open"), cx, |_, _| false));

        Overlay::new(
            (id, "overlay"),
            open_state,
            move |trigger_open, _, cx| {
                let opened = *trigger_open.read(cx);
                base.role(Role::ComboBox)
                    .selected(opened)
                    .disabled(disabled)
                    .rounded_sm()
                    .unified_sized(unified_size)
                    .children(children)
                    .when(!opened && !disabled, |this| {
                        this.on_click(move |_, _, cx| trigger_open.write(cx, true))
                    })
                    .refine_style(&style)
                    .into_any_element()
            },
            move |context, _, _| {
                let open_state = context.open_state;
                let menu = menu
                    .with_unified_size(unified_size)
                    .on_item_click(move |event, id, window, cx| {
                        open_state.write(cx, false);

                        if let Some(on_menu_item_click) = on_menu_item_click.as_ref() {
                            on_menu_item_click(event, id, window, cx);
                        }
                    })
                    .when_some(menu_max_h, |this, max_h| this.max_h(max_h));

                div()
                    .min_w(context.trigger_bounds.size.width)
                    .when_some(menu_max_h, |this, max_h| this.max_h(max_h))
                    // The entries are a menu, which names no selector of its own, so the box
                    // the panel is drawn in is what a test looks at to see the panel at all.
                    .debug_selector(|| "TEST_DROPDOWN_PANEL".into())
                    .child(menu)
                    .into_any_element()
            },
        )
        .placement(placement)
        .align(align)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui_kit::{
        Context, InteractiveElement as _, IntoElement, Modifiers, MouseButton, ParentElement as _,
        Render, StatefulInteractiveElement, Styled as _, TestAppContext, VisualTestContext, Window,
        div,
    };

    use super::Dropdown;
    use crate::{
        button::Button,
        interactive::InteractiveVariantExt,
        menu::{MenuItem, MenuItemAction},
        size::UnifiedSizable,
        style,
    };

    /// A dropdown above a button, both centred: the popup of an *open* dropdown is
    /// centred under its trigger, so this is the button a popup would cover.
    struct Harness {
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();

            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .child(
                    Dropdown::new("test-dropdown")
                        .child("entries")
                        .small()
                        .menu_item(MenuItem::Action(MenuItemAction::new("entry", "entry"))),
                )
                .child(
                    div()
                        .debug_selector(|| "TEST_DROPDOWN_TARGET".into())
                        .child(
                            Button::new("test-target")
                                .tonal()
                                .small()
                                .child("target")
                                .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                        ),
                )
        }
    }

    /// A dropdown whose trigger content names itself, so that a test can press it: the box a
    /// dropdown draws around the caller's content carries no selector otherwise.
    struct TriggerHarness;

    impl Render for TriggerHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().items_center().child(
                Dropdown::new("test-trigger-dropdown")
                    .child(
                        div()
                            .debug_selector(|| "TEST_DROPDOWN_TRIGGER".into())
                            .child("trigger"),
                    )
                    .small()
                    .menu_item(MenuItem::Action(MenuItemAction::new("entry", "entry"))),
            )
        }
    }

    /// The *same* dropdown, with its trigger box taking the pointer explicitly, as a caller
    /// does for a control inside the drag area of a window.
    struct OccludingTriggerHarness;

    impl Render for OccludingTriggerHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().items_center().child(
                Dropdown::new("test-trigger-dropdown")
                    .child(
                        div()
                            .debug_selector(|| "TEST_DROPDOWN_TRIGGER".into())
                            .child("trigger"),
                    )
                    .small()
                    .occlude()
                    .menu_item(MenuItem::Action(MenuItemAction::new("entry", "entry"))),
            )
        }
    }

    /// The same dropdown, with the trigger that every call site passes: a button of the
    /// caller's, holding the icon or the label the trigger draws.
    struct ButtonTriggerHarness;

    impl Render for ButtonTriggerHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().items_center().child(
                Dropdown::new("test-button-trigger-dropdown")
                    .child(
                        Button::new("test-button-trigger")
                            .tonal()
                            .small()
                            .debug_selector(|| "TEST_BUTTON_TRIGGER".into())
                            .child("trigger"),
                    )
                    .small()
                    .menu_item(MenuItem::Action(MenuItemAction::new("entry", "entry"))),
            )
        }
    }

    fn harness(cx: &mut TestAppContext) -> (Rc<Cell<usize>>, &mut VisualTestContext) {
        cx.update(style::theme::install);
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Harness { clicks }
        });
        cx.run_until_parked();

        (clicks, cx)
    }

    fn trigger_harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(style::theme::install);
        let (_, cx) = cx.add_window_view(|_, _| TriggerHarness);
        cx.run_until_parked();

        cx
    }

    fn occluding_trigger_harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(style::theme::install);
        let (_, cx) = cx.add_window_view(|_, _| OccludingTriggerHarness);
        cx.run_until_parked();

        cx
    }

    fn button_trigger_harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(style::theme::install);
        let (_, cx) = cx.add_window_view(|_, _| ButtonTriggerHarness);
        cx.run_until_parked();

        cx
    }

    fn shows(cx: &mut VisualTestContext, selector: &'static str) -> bool {
        cx.debug_bounds(selector).is_some()
    }

    /// Presses what `selector` names, the way a window delivers a press: the button goes
    /// down, the frame that follows is drawn, and only then does it come up.
    ///
    /// The frame in the middle is the point of this helper: the panel closes while the
    /// button is down, so a trigger that opened on the way up would undo exactly that, and
    /// delivering the two without a frame between them hides the difference.
    fn press(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .expect("the harness draws what it is asked to press")
            .center();
        cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// A dropdown that was never opened draws no popup, so a click under its trigger
    /// belongs to whatever is drawn there.
    #[gpui_kit::test]
    fn a_closed_dropdown_should_not_swallow_a_click_behind_it(cx: &mut TestAppContext) {
        let (clicks, cx) = harness(cx);

        let target = cx
            .debug_bounds("TEST_DROPDOWN_TARGET")
            .expect("the harness draws its target");
        cx.simulate_click(target.center(), Modifiers::none());

        assert_eq!(
            clicks.get(),
            1,
            "a dropdown that has never been opened must not cover what is behind it"
        );
    }

    /// Pressing the trigger of an open dropdown is a press outside the panel: it closes,
    /// the way it does for a [`crate::select::Select`], and it does not reopen the panel
    /// on the way up.
    #[gpui_kit::test]
    fn pressing_the_trigger_of_an_open_dropdown_should_close_it(cx: &mut TestAppContext) {
        let cx = trigger_harness(cx);
        assert!(!shows(cx, "TEST_DROPDOWN_PANEL"));

        press(cx, "TEST_DROPDOWN_TRIGGER");
        assert!(
            shows(cx, "TEST_DROPDOWN_PANEL"),
            "pressing the trigger opens the dropdown"
        );

        press(cx, "TEST_DROPDOWN_TRIGGER");

        assert!(
            !shows(cx, "TEST_DROPDOWN_PANEL"),
            "a press on the trigger is a press outside the panel"
        );
    }

    /// A trigger that is itself a button — the shape every call site has, where the caller
    /// passes the icon or the label the trigger draws — is the dropdown's trigger: pressing
    /// it opens the dropdown, and pressing it again closes it.
    ///
    /// The button is drawn inside the dropdown's own button, so this is also what says the
    /// press is not lost on the way: it is on the box the dropdown wraps the caller's
    /// content in, which is where a caller's button cannot answer for it.
    #[gpui_kit::test]
    fn a_button_trigger_should_open_and_close_its_dropdown(cx: &mut TestAppContext) {
        let cx = button_trigger_harness(cx);

        press(cx, "TEST_BUTTON_TRIGGER");
        assert!(
            shows(cx, "TEST_DROPDOWN_PANEL"),
            "a button as the trigger must open the dropdown"
        );

        press(cx, "TEST_BUTTON_TRIGGER");

        assert!(
            !shows(cx, "TEST_DROPDOWN_PANEL"),
            "a button as the trigger must close the dropdown"
        );
    }

    /// Writing to the state is the only way a dropdown opens, so a press that opened nothing
    /// is a press that never arrived. Each of the tests below therefore says the panel is
    /// there before it asserts that a further press took it away: without that, "closed"
    /// would also be satisfied by a dropdown that never opened.
    ///
    /// A trigger box that takes the pointer explicitly — as a caller does for a control
    /// inside the drag area of a window — leaves the dropdown's own press in place.
    ///
    /// That press is on the box around the trigger button rather than on the button itself,
    /// and an occluding descendant is where a hit test stops: were it the caller's content
    /// that occluded and the box around it that answered, this press would never arrive,
    /// and the dropdown would neither open nor close.
    #[gpui_kit::test]
    fn an_occluding_trigger_should_still_open_and_close_its_dropdown(cx: &mut TestAppContext) {
        let cx = occluding_trigger_harness(cx);

        press(cx, "TEST_DROPDOWN_TRIGGER");
        assert!(
            shows(cx, "TEST_DROPDOWN_PANEL"),
            "an occluding trigger must still open the dropdown it belongs to"
        );

        press(cx, "TEST_DROPDOWN_TRIGGER");

        assert!(
            !shows(cx, "TEST_DROPDOWN_PANEL"),
            "an occluding trigger must still close the dropdown it belongs to"
        );
    }
}

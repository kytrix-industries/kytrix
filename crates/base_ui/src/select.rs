use std::rc::Rc;

use gpui_kit::{
    AnyElement, App, ElementId, Empty, InteractiveElement, Interactivity, IntoElement,
    ParentElement, Pixels, RenderOnce, SharedString, StatefulInteractiveElement, StyleRefinement,
    Styled, Transformation, Window,
    base::{Align, Disableable, Easing, Placement, Transition, transition},
    div,
    prelude::FluentBuilder,
    radians,
};
use smallvec::SmallVec;

use crate::{
    animation::ANIMATION_TICK,
    dropdown::Dropdown,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::MenuItem,
    size::{UnifiedSizable, UnifiedSize},
    tooltip::TooltipExt,
};

#[derive(IntoElement)]
pub struct Select {
    id: ElementId,
    base: Dropdown,
    unified_size: UnifiedSize,
    placeholder: AnyElement,
    selected_item_id: Option<SharedString>,
    menu_items: SmallVec<[MenuItem; 8]>,
    on_change: Option<Rc<dyn Fn(&Option<SharedString>, &mut Window, &mut App)>>,
    disabled: bool,
    placement: Placement,
    align: Align,
    menu_max_h: Option<Pixels>,
}

impl Select {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        let base = Dropdown::new(id.clone());

        Self {
            id,
            base,
            unified_size: UnifiedSize::Medium,
            placeholder: Empty.into_any_element(),
            selected_item_id: None,
            menu_items: SmallVec::new(),
            on_change: None,
            disabled: false,
            placement: Placement::Bottom,
            align: Align::Center,
            menu_max_h: None,
        }
    }

    pub fn selected_item(mut self, item_id: Option<impl Into<SharedString>>) -> Self {
        self.selected_item_id = item_id.map(Into::into);
        self
    }

    pub fn placeholder(mut self, placeholder: impl IntoElement) -> Self {
        self.placeholder = placeholder.into_any_element();
        self
    }

    pub fn menu_item(mut self, menu_item: MenuItem) -> Self {
        self.menu_items.push(menu_item);
        self
    }

    pub fn on_change(
        mut self,
        on_change: impl Fn(&Option<SharedString>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Rc::new(on_change));
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

    pub fn menu_max_height(mut self, menu_max_h: impl Into<Pixels>) -> Self {
        self.menu_max_h = Some(menu_max_h.into());
        self
    }
}

impl Styled for Select {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for Select {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Select {}

impl InteractiveVariantExt for Select {
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

impl Disableable for Select {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl UnifiedSizable for Select {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = size;
        self
    }
}

impl TooltipExt for Select {
    fn with_tooltip(mut self, tooltip: impl IntoElement) -> Self {
        self.base = self.base.with_tooltip(tooltip);
        self
    }
}

impl RenderOnce for Select {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            base,
            unified_size,
            placeholder,
            selected_item_id,
            menu_items,
            on_change,
            disabled,
            placement,
            align,
            menu_max_h,
        } = self;

        let open_state = window.use_keyed_state((id.clone(), "open"), cx, |_, _| false);
        let unfolded = transition(
            (id, "caret"),
            if *open_state.read(cx) && !disabled {
                1.0
            } else {
                0.0
            },
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let selected = selected_item_id.as_ref().and_then(|id| {
            menu_items.iter().find_map(|item| match item {
                MenuItem::Action(action) if action.id.eq(id) => Some((None, action.label.clone())),
                MenuItem::IconAction(action) if action.id.eq(id) => {
                    Some((Some(action.icon_kind), action.label.clone()))
                }
                _ => None,
            })
        });

        menu_items
            .into_iter()
            .fold(base, |base, item| base.menu_item(item))
            .when_none(&selected, |this| {
                this.child(div().flex_1().child(placeholder))
            })
            .when_some(selected, |this, (icon_kind, label)| {
                this.when_some(icon_kind, |this, icon_kind| {
                    this.child(Icon::new(icon_kind).size(unified_size.icon_size()))
                })
                .child(div().flex_1().child(label))
            })
            .child(
                Icon::new(IconKind::CaretDown)
                    .size(unified_size.icon_size())
                    .transformation(Transformation::rotate(radians(
                        -std::f32::consts::PI * unfolded,
                    ))),
            )
            .when_some(menu_max_h, |this, menu_max_h| this.menu_max_h(menu_max_h))
            .disabled(disabled)
            .with_unified_size(unified_size)
            .placement(placement)
            .align(align)
            .open_state(&open_state)
            .on_menu_item_click(move |_, id, window, cx| {
                open_state.write(cx, false);
                if let Some(on_change) = on_change.clone().filter(|_| {
                    selected_item_id
                        .as_ref()
                        .is_none_or(|selected_id| id.ne(selected_id))
                }) {
                    on_change(&Some(id), window, cx)
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui_kit::{
        Context, InteractiveElement, IntoElement, Modifiers, MouseButton, ParentElement, Render,
        StatefulInteractiveElement, Styled, TestAppContext, VisualTestContext, Window, div,
        test::TestWindowExt as _,
    };

    use super::Select;
    use crate::{
        animation::ANIMATION_TICK,
        button::Button,
        interactive::InteractiveVariantExt,
        menu::{MenuItem, MenuItemAction},
        size::UnifiedSizable,
        style,
    };

    /// A select above a button, both centred: the popup of an *open* select is centred
    /// under its trigger, so this is the button a popup would cover.
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
                    Select::new("test-select")
                        .small()
                        .placeholder("nothing chosen")
                        .menu_item(MenuItem::Action(MenuItemAction::new("entry", "entry"))),
                )
                .child(
                    div().debug_selector(|| "TEST_SELECT_TARGET".into()).child(
                        Button::new("test-target")
                            .tonal()
                            .small()
                            .child("target")
                            .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                    ),
                )
        }
    }

    /// A select with nothing to choose from: the panel it opens holds no entries, and the one
    /// thing it has to say is the placeholder it draws on its own trigger.
    struct EmptyHarness;

    impl Render for EmptyHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().items_center().child(
                Select::new("test-empty-select")
                    .small()
                    .placeholder("nothing to choose from")
                    .aria_label("nothing to choose from")
                    .debug_selector(|| "TEST_EMPTY_SELECT".into()),
            )
        }
    }

    /// A select that was never opened draws no popup, so a click under its trigger
    /// belongs to whatever is drawn there.
    #[gpui_kit::test]
    fn a_closed_select_should_not_swallow_a_click_behind_it(cx: &mut TestAppContext) {
        cx.update(style::theme::install);
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Harness { clicks }
        });
        cx.run_until_parked();

        let target = cx
            .debug_bounds("TEST_SELECT_TARGET")
            .expect("the harness draws its target");
        cx.simulate_click(target.center(), Modifiers::none());

        assert_eq!(
            clicks.get(),
            1,
            "a select that has never been opened must not cover what is behind it"
        );
    }

    /// A select drawn for the first time points where it already is: its caret turns when the
    /// menu opens or closes, not when the select appears.
    ///
    /// The one frame asked for here is the popup's own: a menu that has never been shown is
    /// reported by `Presence` as exiting, and it runs that exit. The caret must add nothing to it,
    /// which is what this counts.
    #[gpui_kit::test]
    fn a_select_drawn_for_the_first_time_should_not_turn_its_caret(cx: &mut TestAppContext) {
        cx.update(style::theme::install);
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view(move |_, _| Harness { clicks });
        cx.run_until_parked();

        let start = turning(cx);
        cx.executor().advance_clock(ANIMATION_TICK * 2);
        let rest = turning(cx);

        assert_eq!(rest, 0, "a select at rest must not keep drawing");
        assert!(
            start <= 1,
            "only the popup a select has yet to appear may ask for a frame at first: a caret \
             drawn for the first time belongs where its menu already is"
        );
    }

    /// Pressing the trigger of an open select is a press outside its menu: it closes, and it
    /// does not open the menu again on the way up.
    ///
    /// This is the behaviour a [`crate::dropdown::Dropdown`] is held to as well, so that the two
    /// controls that open a menu answer the same press the same way.
    ///
    /// The panel a select opens is the one [`crate::dropdown::Dropdown`] draws, so the box that
    /// panel is drawn in is where its being open is read from: the entries menu names no
    /// selector of its own.
    #[gpui_kit::test]
    fn pressing_the_trigger_of_an_open_select_should_close_it(cx: &mut TestAppContext) {
        cx.update(style::theme::install);
        let (_, cx) = cx.add_window_view(|_, _| EmptyHarness);
        cx.run_until_parked();
        assert!(cx.debug_bounds("TEST_DROPDOWN_PANEL").is_none());

        press(cx, "TEST_EMPTY_SELECT");
        assert!(
            cx.debug_bounds("TEST_DROPDOWN_PANEL").is_some(),
            "pressing the trigger opens the select"
        );

        press(cx, "TEST_EMPTY_SELECT");

        assert!(
            cx.debug_bounds("TEST_DROPDOWN_PANEL").is_none(),
            "a press on the trigger is a press outside the menu"
        );
    }

    /// Presses what `selector` names, the way a window delivers a press: the button goes down,
    /// the frame that follows is drawn, and only then does it come up.
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

    /// How many elements are turning: the frames already asked for are delivered first, and then
    /// one frame is drawn and its requests are counted, so the count is of one frame alone.
    fn turning(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            window.simulate_next_frame(cx)
        })
    }
}

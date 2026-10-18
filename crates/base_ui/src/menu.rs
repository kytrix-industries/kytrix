use std::rc::Rc;

use either::Either;
use gpui_kit::{
    base::{Disableable, ScrollbarMode, StyledExt, box_shadow},
    prelude::FluentBuilder,
    *,
};
use smallvec::SmallVec;

use crate::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    scroll_bar::Scrollbar,
    separator::Separator,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
};

pub struct MenuItemAction {
    pub id: SharedString,
    pub label: SharedString,
    disabled: bool,
}

impl Disableable for MenuItemAction {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl MenuItemAction {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            disabled: false,
        }
    }
}

pub struct MenuItemIconAction {
    pub id: SharedString,
    pub label: SharedString,
    pub icon_kind: IconKind,
    disabled: bool,
}

impl Disableable for MenuItemIconAction {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl MenuItemIconAction {
    pub fn new(
        id: impl Into<SharedString>,
        icon_kind: IconKind,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon_kind,
            disabled: false,
        }
    }
}

pub struct MenuItemGroup {
    label: SharedString,
}

impl MenuItemGroup {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

pub enum MenuItem {
    Custom(AnyElement),
    Action(MenuItemAction),
    IconAction(MenuItemIconAction),
    Group(MenuItemGroup),
    Separator,
}

#[derive(IntoElement)]
pub struct Menu {
    id: ElementId,
    unified_size: UnifiedSize,
    children: SmallVec<[MenuItem; 8]>,
    on_item_click: Option<Rc<dyn Fn(&ClickEvent, SharedString, &mut Window, &mut App)>>,
    style: StyleRefinement,
}

impl Menu {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            unified_size: UnifiedSize::Medium,
            children: SmallVec::new(),
            on_item_click: None,
            style: StyleRefinement::default(),
        }
    }

    pub fn item(mut self, item: MenuItem) -> Self {
        self.children.push(item);
        self
    }

    pub fn on_item_click(
        mut self,
        on_item_click: impl Fn(&ClickEvent, SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_item_click = Some(Rc::new(on_item_click));
        self
    }
}

impl Styled for Menu {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl UnifiedSizable for Menu {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = size;
        self
    }
}

impl RenderOnce for Menu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Menu {
            id,
            unified_size,
            children,
            on_item_click,
            style,
        } = self;

        let keep_icon_placeholder = children
            .iter()
            .any(|child| matches!(child, MenuItem::IconAction(_)));

        let scroll_handle_state =
            window.use_keyed_state((id.clone(), "scroll-handle"), cx, |_, _| {
                ScrollHandle::new()
            });
        let scroll_handle = scroll_handle_state.read(cx).clone();

        let theme = cx.theme();

        let inner = div()
            .relative()
            .size_full()
            .child(
                div()
                    .id(id.clone())
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(unified_size.gap())
                    .p_1()
                    .overflow_scroll()
                    .track_scroll(&scroll_handle)
                    .children(children.into_iter().map(|child| {
                        match child {
                            MenuItem::Custom(element) => element,
                            MenuItem::Action(action) => MenuButton {
                                id: (id.clone(), action.id.clone()).into(),
                                unified_size,
                                item_id: action.id,
                                icon_kind: Either::Right(keep_icon_placeholder),
                                label: action.label,
                                disabled: action.disabled,
                                on_click: {
                                    let on_item_click = on_item_click.clone();
                                    move |event, item_id, window, cx| {
                                        if let Some(on_item_click) = on_item_click.as_ref() {
                                            on_item_click(event, item_id, window, cx);
                                        }
                                    }
                                },
                            }
                            .into_any_element(),
                            MenuItem::IconAction(icon_action) => MenuButton {
                                id: (id.clone(), icon_action.id.clone()).into(),
                                unified_size,
                                item_id: icon_action.id,
                                icon_kind: Either::Left(icon_action.icon_kind),
                                label: icon_action.label,
                                disabled: icon_action.disabled,
                                on_click: {
                                    let on_item_click = on_item_click.clone();
                                    move |event, item_id, window, cx| {
                                        if let Some(on_item_click) = on_item_click.as_ref() {
                                            on_item_click(event, item_id, window, cx);
                                        }
                                    }
                                },
                            }
                            .into_any_element(),
                            // A group heading is not a control: it takes no pointer
                            // input, so it can never be mistaken for a selectable entry.
                            MenuItem::Group(group) => div()
                                .p(unified_size.px())
                                .text_size(unified_size.text_size())
                                .text_color(theme.components.menu.group_foreground.hsla)
                                .child(group.label)
                                .into_any_element(),
                            MenuItem::Separator => Separator::horizontal().into_any_element(),
                        }
                    })),
            )
            .child(Scrollbar::vertical(scroll_handle).mode(ScrollbarMode::Always));

        let shadow = theme.components.menu.shadow.hsla;

        // §8.3: a menu is a flat popover, so it casts a compact contact shadow rather than the
        // dialog's wide elevation — a close band under the panel, plus a short falloff. The
        // `line.divider` edge it also draws is what keeps the panel legible on the dark
        // appearance, where a black shadow over a near-black page has little to say.
        div()
            .size_full()
            .bg(theme.components.menu.background.hsla)
            .rounded_sm()
            .border_1()
            .border_color(theme.roles.line.divider.hsla)
            .shadow(vec![
                box_shadow(0., 1., 2., 0., shadow),
                box_shadow(0., 3., 6., -2., shadow),
            ])
            .child(inner)
            .refine_style(&style)
    }
}

#[derive(IntoElement)]
struct MenuButton<H>
where
    H: Fn(&ClickEvent, SharedString, &mut Window, &mut App) + 'static,
{
    id: ElementId,
    unified_size: UnifiedSize,
    item_id: SharedString,
    icon_kind: Either<IconKind, bool>,
    label: SharedString,
    disabled: bool,
    on_click: H,
}

impl<H> RenderOnce for MenuButton<H>
where
    H: Fn(&ClickEvent, SharedString, &mut Window, &mut App) + 'static,
{
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let MenuButton {
            id,
            unified_size,
            item_id,
            icon_kind,
            label,
            disabled,
            on_click,
        } = self;

        Button::new(id)
            .ghost()
            .with_unified_size(unified_size)
            .disabled(disabled)
            // An entry is drawn by a button, which names no selector of its own: the entry's
            // own id is what a test looks at to see that the menu offers it at all.
            .debug_selector({
                let item_id = item_id.clone();
                move || format!("TEST_MENU_ITEM_{item_id}")
            })
            .when(
                matches!(icon_kind, Either::Left(_) | Either::Right(true)),
                |this| {
                    this.child(if let Either::Left(icon_kind) = icon_kind {
                        Icon::new(icon_kind)
                            .size(unified_size.icon_size())
                            .into_any_element()
                    } else {
                        div().size(unified_size.icon_size()).into_any_element()
                    })
                },
            )
            .child(label)
            .when(!disabled, |this| {
                this.on_click(move |event, window, cx| on_click(event, item_id.clone(), window, cx))
            })
    }
}

//! A single menu popup owned by the window layer.
//!
//! A context menu is a menu, not a control with a trigger: the caller decides where it
//! opens and when it goes away, and the layer owns it for as long as it is up. That is
//! what makes it different from [`crate::overlay::Overlay`]: a dropdown belongs to the
//! element that opened it and is placed against it, while a context menu is a window-level
//! surface that is drawn over everything and can be asked about by anything else in the
//! window — the floating sidebar, for one, holds itself up while a menu is open.
//!
//! The menu is drawn over a transparent sheet that covers the window. The sheet takes the
//! mouse: a press outside the menu closes it and never reaches what is behind it. The menu
//! itself is a sibling of the sheet's backdrop, so a press on an entry belongs to the entry.
//!
//! The menu is positioned by its anchor corner: [`ContextMenu::new`] takes a point and a
//! width, and [`ContextMenu::anchor`] chooses which corner of the menu sits on that point.
//! Its height is the height of the entries. A context menu opened at a press uses
//! [`gpui_kit::Anchor::TopLeft`]; a menu opened beside a row whose bottom is the anchor
//! uses [`gpui_kit::Anchor::BottomLeft`].
//!
//! Only one menu is up at a time: opening another replaces it, and the layer closes the
//! one it replaces.

use std::rc::Rc;

use gpui_kit::{
    Anchor, AnyElement, App, ClickEvent, Context, ElementId, Empty, InteractiveElement,
    IntoElement, ParentElement, Pixels, Point, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Window,
    base::{POPUP_PRIORITY, Positioner},
    deferred, div,
    prelude::FluentBuilder,
    px,
};

use crate::{layer::LayerExt as _, menu::Menu};

/// The panel of a context menu, named for tests.
pub const CONTEXT_MENU: &str = "BASE_UI_CONTEXT_MENU";

/// The transparent sheet under a context menu, named for tests.
pub const CONTEXT_MENU_BACKDROP: &str = "BASE_UI_CONTEXT_MENU_BACKDROP";

/// The distance a context menu keeps from the window edge.
const CONTEXT_MENU_MARGIN: Pixels = px(4.0);

/// How far above the other deferred surfaces a context menu is drawn.
///
/// A tooltip is one step higher, so the hint on an entry inside the menu is not buried
/// under the menu it belongs to.
const CONTEXT_MENU_PRIORITY: usize = POPUP_PRIORITY + 1;

/// Builds the menu's entries.
///
/// It is called each time the menu is drawn, so the entries are built from the state that
/// is current then rather than from a snapshot taken when the menu opened. It is not called
/// while the menu is closed.
type MenuBuilder = Box<dyn Fn(&mut Window, &mut App) -> Menu>;

/// The action an entry asks for, called after the menu has closed.
type MenuItemClick = Rc<dyn Fn(&ClickEvent, SharedString, &mut Window, &mut App)>;

/// Called once when the menu closes, whichever way it closed.
type MenuClose = Rc<dyn Fn(&mut App)>;

/// A menu opened at a point and owned by the window's layer.
///
/// It is not a control: it has no trigger and no open state of its own that a caller
/// writes. [`crate::layer::LayerExt::context_menu`] puts it on the layer, and a press on
/// the sheet, an entry, or the layer's own dismissal takes it down.
///
/// Its height follows its entries. A caller with more entries than fit in the window should
/// cap the [`Menu`] with [`Menu::max_h`], which makes the entries scroll inside the menu.
pub struct ContextMenu {
    id: ElementId,
    position: Point<Pixels>,
    anchor: Anchor,
    width: Pixels,
    menu: Option<MenuBuilder>,
    on_item_click: Option<MenuItemClick>,
    on_close: Option<MenuClose>,
    aria_label: Option<SharedString>,
    debug_selector: Option<&'static str>,
    open: bool,
}

impl ContextMenu {
    /// A menu of `width` whose `anchor` corner sits at `position`.
    ///
    /// `menu` builds the entries on every frame the menu is drawn. The menu's
    /// `on_item_click` is set by this component: an entry click closes the menu first, and
    /// then reports the entry through [`Self::on_item_click`]. A builder that sets its own
    /// `on_item_click` has it replaced.
    pub fn new(
        id: impl Into<ElementId>,
        position: Point<Pixels>,
        width: Pixels,
        menu: impl Fn(&mut Window, &mut App) -> Menu + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            position,
            anchor: Anchor::TopLeft,
            width,
            menu: Some(Box::new(menu)),
            on_item_click: None,
            on_close: None,
            aria_label: None,
            debug_selector: None,
            open: true,
        }
    }

    /// Which corner of the menu sits on the point.
    ///
    /// The default is `TopLeft`, the shape a menu opened at a press has. A menu opened
    /// beside the bottom of a row uses `BottomLeft`, so it grows upward from the row's
    /// bottom rather than below it.
    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// The name assistive technology reads for the menu.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }

    /// Names the menu's panel for tests. Without it the panel is [`CONTEXT_MENU`].
    pub fn debug_selector(mut self, selector: &'static str) -> Self {
        self.debug_selector = Some(selector);
        self
    }

    /// Reports an entry after the menu has closed.
    ///
    /// The menu closes before this is called, so an action that opens a dialog opens it
    /// over a menu that is already gone.
    pub fn on_item_click(
        mut self,
        on_item_click: impl Fn(&ClickEvent, SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_item_click = Some(Rc::new(on_item_click));
        self
    }

    /// Reports that the menu closed, whichever way it closed.
    ///
    /// It is called once, when the menu stops being open. The dismissal can happen inside
    /// the layer's own update (a replacement, a dialog), so this is for clearing the
    /// trigger's state and not for opening another menu or dialog.
    pub fn on_close(mut self, on_close: impl Fn(&mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(on_close));
        self
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Takes the menu down and reports it through [`Self::on_close`].
    ///
    /// The entity stays on the layer, closed and empty, until another menu replaces it.
    /// Dropping it while its own press is being dispatched would remove a view the frame
    /// is still drawing.
    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }

        self.open = false;
        self.menu.take();
        self.on_item_click.take();
        let on_close = self.on_close.take();
        cx.notify();

        if let Some(on_close) = on_close {
            on_close(cx);
        }
    }

    /// The entries, with the click that closes the menu wrapped around the caller's action.
    fn entries(&self, window: &mut Window, cx: &mut App) -> Option<Menu> {
        let builder = self.menu.as_ref()?;
        let on_item_click = self.on_item_click.clone();
        let menu = builder(window, cx);

        Some(menu.on_item_click(move |event, item_id, window, cx| {
            window.dismiss_context_menu(cx);
            if let Some(on_item_click) = on_item_click.as_ref() {
                on_item_click(event, item_id, window, cx);
            }
        }))
    }

    /// The sheet every press outside the menu lands on.
    fn backdrop(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let viewport = window.viewport_size();

        div()
            .id((self.id.clone(), "backdrop"))
            .absolute()
            .top_0()
            .left_0()
            .w(viewport.width)
            .h(viewport.height)
            .occlude()
            .debug_selector(|| CONTEXT_MENU_BACKDROP.into())
            // The wheel belongs to the menu, not to the page under the sheet: an event the
            // menu's own scroll container did not take stops here.
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.dismiss(cx);
            }))
            .into_any_element()
    }

    /// The menu itself, placed by its anchor corner.
    fn panel(&self, menu: Menu) -> AnyElement {
        let selector = self.debug_selector.unwrap_or(CONTEXT_MENU);
        let panel = div()
            .id((self.id.clone(), "panel"))
            .role(Role::Menu)
            .when_some(self.aria_label.clone(), |this, label| {
                this.aria_label(label)
            })
            .debug_selector(move || selector.to_owned())
            .w(self.width)
            // The wheel is the menu's while the pointer is inside it. The entries scroll
            // first; a wheel over the panel's own margin stops here rather than reaching
            // the page.
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .child(menu);

        Positioner::corner(self.anchor, self.position)
            .margin(CONTEXT_MENU_MARGIN)
            .occlude()
            .child(panel)
            .into_any_element()
    }
}

impl Render for ContextMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        let Some(menu) = self.entries(window, cx) else {
            return Empty.into_any_element();
        };

        let viewport = window.viewport_size();
        let backdrop = self.backdrop(window, cx);
        let panel = self.panel(menu);
        let sheet = div()
            .absolute()
            .top_0()
            .left_0()
            .w(viewport.width)
            .h(viewport.height)
            .child(backdrop)
            .child(panel);

        deferred(sheet)
            .with_priority(CONTEXT_MENU_PRIORITY)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui_kit::{
        AppContext as _, Context, DismissEvent, Entity, EventEmitter, InteractiveElement,
        IntoElement, Modifiers, MouseButton, ParentElement as _, Render, ScrollDelta, ScrollEvent,
        ScrollHandle, SharedString, StatefulInteractiveElement, Styled as _, TestAppContext,
        VisualTestContext, Window, div, point, px, size,
    };
    use smol_str::format_smolstr;

    use super::{CONTEXT_MENU_BACKDROP, ContextMenu};
    use crate::{
        button::Button,
        interactive::InteractiveVariantExt,
        layer::{Layer, LayerExt as _},
        menu::{Menu, MenuItem, MenuItemAction},
        size::UnifiedSizable,
        style,
    };

    const TARGET: &str = "TEST_CONTEXT_MENU_TARGET";
    const PANEL: &str = "TEST_CONTEXT_MENU";
    const ACTION: &str = "TEST_MENU_ITEM_action";

    /// A button behind the menu, which a click must not reach while the menu is up.
    struct Backdrop {
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Backdrop {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();

            div().size_full().child(
                div()
                    .absolute()
                    .top(px(10.0))
                    .left(px(10.0))
                    .debug_selector(|| TARGET.into())
                    .child(
                        Button::new("context-menu-target")
                            .tonal()
                            .small()
                            .child("target")
                            .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                    ),
            )
        }
    }

    /// A dialog content that is never dismissed by the test: the dialog exists so that the
    /// context menu has something modal to be taken down for.
    struct DialogContent;

    impl EventEmitter<DismissEvent> for DialogContent {}

    impl Render for DialogContent {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Button::new("context-menu-dialog-content")
                .tonal()
                .small()
                .child("dialog")
        }
    }

    /// A tall page with a scroll handle, so a test can say whether the wheel under the
    /// menu's sheet reaches the page.
    struct ScrollingBackdrop {
        scroll: ScrollHandle,
    }

    impl Render for ScrollingBackdrop {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .id("context-menu-scrolling-page")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().h(px(2000.0))),
            )
        }
    }

    fn harness(cx: &mut TestAppContext) -> (Rc<Cell<usize>>, &mut VisualTestContext) {
        cx.update(style::theme::install);
        let clicks = Rc::new(Cell::new(0));
        let rendered = clicks.clone();
        let (_, cx) =
            cx.add_window_view(move |_, cx| Layer::new(cx.new(|_| Backdrop { clicks: rendered })));
        cx.simulate_resize(size(px(400.0), px(400.0)));
        cx.run_until_parked();

        (clicks, cx)
    }

    fn menu() -> Menu {
        Menu::new("test-context-menu-list")
            .item(MenuItem::Action(MenuItemAction::new("action", "Action")))
    }

    fn capped_menu() -> Menu {
        let mut menu = Menu::new("test-capped-context-menu-list").max_h(px(80.0));
        for index in 0..8 {
            menu = menu.item(MenuItem::Action(MenuItemAction::new(
                SharedString::new(format_smolstr!("item-{index}")),
                SharedString::new(format_smolstr!("Item {index}")),
            )));
        }
        menu
    }

    fn scroll_harness(cx: &mut TestAppContext) -> (ScrollHandle, &mut VisualTestContext) {
        cx.update(style::theme::install);
        let scroll = ScrollHandle::new();
        let rendered = scroll.clone();
        let (_, cx) = cx.add_window_view(move |_, cx| {
            Layer::new(cx.new(|_| ScrollingBackdrop { scroll: rendered }))
        });
        cx.simulate_resize(size(px(400.0), px(400.0)));
        cx.run_until_parked();

        (scroll, cx)
    }

    fn wheel(cx: &mut VisualTestContext, position: gpui_kit::Point<gpui_kit::Pixels>, lines: f32) {
        cx.simulate_mouse_move(position, None, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_event(ScrollEvent {
            position,
            delta: ScrollDelta::Lines(point(0.0, lines)),
            ..Default::default()
        });
        cx.run_until_parked();
    }

    /// Opens a menu through the window layer, reporting what its entry and its close ask for.
    fn open_menu(
        cx: &mut VisualTestContext,
        id: &'static str,
        at: gpui_kit::Point<gpui_kit::Pixels>,
        on_action: Rc<Cell<usize>>,
        on_close: Rc<Cell<usize>>,
    ) {
        cx.update(|window, cx| {
            window.context_menu(
                cx,
                ContextMenu::new(id, at, px(120.0), |_, _| menu())
                    .debug_selector(PANEL)
                    .on_item_click(move |_, _, _, _| {
                        on_action.set(on_action.get() + 1);
                    })
                    .on_close(move |_| on_close.set(on_close.get() + 1)),
            );
        });
        cx.run_until_parked();
    }

    fn close_counts() -> (Rc<Cell<usize>>, Rc<Cell<usize>>) {
        (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)))
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        cx.run_until_parked();
    }

    fn press(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the window should draw {selector}"))
            .center();
        cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        // The menu may have gone away on the down; draw that frame before the release, so
        // the test can see a release that lands on a different surface.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    fn shows(cx: &mut VisualTestContext, selector: &'static str) -> bool {
        cx.debug_bounds(selector).is_some()
    }

    /// The transparent sheet takes the press: the button under it is not clicked, and the
    /// menu closes instead.
    #[gpui_kit::test]
    fn a_context_menu_should_block_the_surface_behind_it(cx: &mut TestAppContext) {
        let (clicks, cx) = harness(cx);
        let (action, close) = close_counts();
        open_menu(
            cx,
            "blocks",
            point(px(200.0), px(200.0)),
            action,
            close.clone(),
        );

        assert!(shows(cx, PANEL));
        assert!(shows(cx, CONTEXT_MENU_BACKDROP));
        assert!(shows(cx, TARGET));

        press(cx, TARGET);

        assert_eq!(
            clicks.get(),
            0,
            "the sheet must not let the press reach the button behind it"
        );
        assert!(!shows(cx, PANEL), "the press outside the menu closes it");
        assert_eq!(close.get(), 1, "the close is reported once");
        assert!(
            !cx.update(|window, cx| window.has_context_menu(cx)),
            "a closed menu is not an open context menu"
        );
    }

    /// An entry reports its action after the menu is gone, and the sheet's press does not
    /// run the button behind it either.
    #[gpui_kit::test]
    fn choosing_an_entry_should_close_the_menu_and_run_its_action(cx: &mut TestAppContext) {
        let (clicks, cx) = harness(cx);
        let (action, close) = close_counts();
        open_menu(
            cx,
            "entry",
            point(px(200.0), px(200.0)),
            action.clone(),
            close.clone(),
        );

        click(cx, ACTION);

        assert_eq!(action.get(), 1, "the entry reports its action");
        assert_eq!(close.get(), 1, "the menu closes before the action");
        assert!(!shows(cx, PANEL));
        assert_eq!(
            clicks.get(),
            0,
            "the button behind the sheet was not reached"
        );
    }

    /// One menu at a time: a second opening closes the first before the second is drawn.
    #[gpui_kit::test]
    fn a_second_context_menu_should_replace_the_first(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);
        let (first_action, first_close) = close_counts();
        let (second_action, second_close) = close_counts();

        open_menu(
            cx,
            "first",
            point(px(100.0), px(100.0)),
            first_action.clone(),
            first_close.clone(),
        );
        open_menu(
            cx,
            "second",
            point(px(200.0), px(200.0)),
            second_action.clone(),
            second_close.clone(),
        );

        assert!(
            cx.update(|window, cx| window.has_context_menu(cx)),
            "the second menu is open"
        );
        assert_eq!(first_close.get(), 1, "the replaced menu is closed");
        assert_eq!(
            second_close.get(),
            0,
            "the menu that replaced it is still up"
        );
        assert_eq!(first_action.get(), 0);
        assert_eq!(second_action.get(), 0);

        click(cx, ACTION);

        assert_eq!(second_action.get(), 1, "the second menu's entry ran");
        assert_eq!(first_action.get(), 0, "the first menu's action did not run");
        assert_eq!(second_close.get(), 1);
    }

    /// The menu's anchor corner sits on the point it was opened at, and its width is the
    /// width it was built with. Its height follows the entries.
    #[gpui_kit::test]
    fn a_context_menu_should_open_at_its_anchor_with_its_width(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);
        let (action, close) = close_counts();
        open_menu(cx, "anchor", point(px(100.0), px(100.0)), action, close);

        let panel = cx.debug_bounds(PANEL).expect("the menu should be drawn");

        assert_eq!(panel.origin, point(px(100.0), px(100.0)));
        assert_eq!(panel.size.width, px(120.0));
        assert!(
            panel.size.height > px(0.0),
            "the entries give the menu height"
        );
    }

    /// A menu anchored by its bottom-left grows upward from that point, which is how the
    /// account menu at the foot of the sidebar opens.
    #[gpui_kit::test]
    fn a_bottom_left_menu_should_grow_up_from_its_anchor(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);
        let (action, close) = close_counts();
        cx.update(|window, cx| {
            window.context_menu(
                cx,
                ContextMenu::new("bottom", point(px(200.0), px(200.0)), px(120.0), |_, _| {
                    menu()
                })
                .anchor(gpui_kit::Anchor::BottomLeft)
                .debug_selector(PANEL)
                .on_item_click(move |_, _, _, _| action.set(action.get() + 1))
                .on_close(move |_| close.set(close.get() + 1)),
            );
        });
        cx.run_until_parked();

        let panel = cx.debug_bounds(PANEL).expect("the menu should be drawn");

        assert_eq!(panel.bottom(), px(200.0));
        assert_eq!(panel.left(), px(200.0));
        assert!(
            panel.top() < px(200.0),
            "the menu grows upward from its bottom-left anchor"
        );
    }

    /// A menu is clamped into the window again when the window shrinks: its stored point
    /// can be outside the new viewport.
    #[gpui_kit::test]
    fn a_context_menu_should_stay_inside_a_resized_window(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);
        let (action, close) = close_counts();
        open_menu(cx, "resize", point(px(200.0), px(200.0)), action, close);

        cx.simulate_resize(size(px(250.0), px(250.0)));
        cx.run_until_parked();

        let panel = cx
            .debug_bounds(PANEL)
            .expect("the menu should stay drawn after the resize");
        assert!(
            panel.right() <= px(250.0),
            "the menu is clamped into the resized window's width: {panel:?}"
        );
        assert!(
            panel.bottom() <= px(250.0),
            "the menu is clamped into the resized window's height: {panel:?}"
        );

        // Growing the window again grows the sheet with it: a press in the newly exposed
        // area is the menu's, not the page's.
        cx.simulate_resize(size(px(500.0), px(500.0)));
        cx.run_until_parked();
        let exposed = point(px(450.0), px(450.0));
        cx.simulate_click(exposed, Modifiers::none());
        cx.run_until_parked();
        assert!(
            !shows(cx, PANEL),
            "the full-window sheet should cover the area the grow exposed"
        );
    }

    /// The caller can take the menu down, and the close is reported once even if it is
    /// asked for twice.
    #[gpui_kit::test]
    fn a_menu_should_close_when_its_caller_asks(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);
        let (_, close) = close_counts();
        open_menu(
            cx,
            "dismiss",
            point(px(200.0), px(200.0)),
            Rc::default(),
            close.clone(),
        );

        cx.update(|window, cx| window.dismiss_context_menu(cx));
        cx.update(|window, cx| window.dismiss_context_menu(cx));
        cx.run_until_parked();

        assert!(!shows(cx, PANEL));
        assert_eq!(close.get(), 1, "the close is reported once");
    }

    /// A dialog is modal, so it takes the menu down before it is opened; the menu would
    /// otherwise be drawn over it and take the presses meant for it.
    #[gpui_kit::test]
    fn a_dialog_should_take_the_context_menu_down(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx);
        let (_, close) = close_counts();
        open_menu(
            cx,
            "dialog",
            point(px(200.0), px(200.0)),
            Rc::default(),
            close.clone(),
        );

        cx.update(|window, cx| {
            let content: Entity<DialogContent> = cx.new(|_| DialogContent);
            window.dialog(cx, content);
        });
        cx.run_until_parked();

        assert!(!shows(cx, PANEL), "the dialog takes the menu down");
        assert!(
            !cx.update(|window, cx| window.has_context_menu(cx)),
            "the layer holds no open context menu after the dialog is opened"
        );
        assert_eq!(close.get(), 1, "the menu reports that it closed");
    }

    /// The wheel belongs to the menu while it is open: a scroll over the sheet does not
    /// scroll the page behind it, and the page scrolls again once the menu is gone.
    #[gpui_kit::test]
    fn the_page_should_not_scroll_under_an_open_context_menu(cx: &mut TestAppContext) {
        let (scroll, cx) = scroll_harness(cx);

        wheel(cx, point(px(300.0), px(300.0)), -3.0);
        let before = scroll.offset().y;
        assert!(
            before != px(0.0),
            "the page should scroll while no menu is open"
        );

        let (_, close) = close_counts();
        open_menu(
            cx,
            "scroll",
            point(px(100.0), px(100.0)),
            Rc::default(),
            close,
        );

        wheel(cx, point(px(300.0), px(300.0)), -3.0);
        assert_eq!(
            scroll.offset().y,
            before,
            "a wheel over the sheet must not scroll the page under it"
        );

        let panel = cx
            .debug_bounds(PANEL)
            .expect("the menu should be drawn over the scrolling page");
        wheel(cx, panel.center(), -3.0);
        assert_eq!(
            scroll.offset().y,
            before,
            "a wheel on the menu itself must not scroll the page under it"
        );

        cx.update(|window, cx| window.dismiss_context_menu(cx));
        cx.run_until_parked();
        wheel(cx, point(px(300.0), px(300.0)), -3.0);
        assert!(
            scroll.offset().y != before,
            "the page scrolls again once the menu is gone"
        );
    }

    /// A capped menu scrolls its own entries when the wheel is on the panel; the panel's
    /// scroll stop runs after the menu's scroll container, so the page under it does not move.
    #[gpui_kit::test]
    fn a_capped_context_menu_should_scroll_its_entries(cx: &mut TestAppContext) {
        let (scroll, cx) = scroll_harness(cx);
        let page_before = scroll.offset().y;

        cx.update(|window, cx| {
            window.context_menu(
                cx,
                ContextMenu::new("capped", point(px(100.0), px(100.0)), px(160.0), |_, _| {
                    capped_menu()
                })
                .debug_selector(PANEL),
            );
        });
        cx.run_until_parked();

        let panel = cx
            .debug_bounds(PANEL)
            .expect("the capped menu should be drawn");
        let first = cx
            .debug_bounds("TEST_MENU_ITEM_item-0")
            .expect("the first entry should be drawn before the wheel")
            .top();

        wheel(cx, panel.center(), -3.0);

        let after = cx
            .debug_bounds("TEST_MENU_ITEM_item-0")
            .map(|bounds| bounds.top());
        assert!(
            after.is_none_or(|top| top < first),
            "the wheel should scroll the menu's entries: {first:?} -> {after:?}"
        );
        assert_eq!(
            scroll.offset().y,
            page_before,
            "scrolling the menu must not scroll the page under it"
        );
    }
}

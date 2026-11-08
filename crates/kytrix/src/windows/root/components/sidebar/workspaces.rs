//! The workspace section of the sidebar: the button that adds a workspace, the list of
//! them, and the alerts that name, rename and delete one.
//!
//! The section owns nothing. It reads the list to draw itself and calls back into
//! [`Workspaces`] to change it, which keeps that entity the only place that knows what a
//! workspace is. It is a section of its own rather than part of the panel so that the list
//! can be mounted — and tested — without the settings entry at the panel's foot.

use app_config::workspace::validate_workspace_name;
use base_ui::{
    animation::ANIMATION_TICK,
    button::Button,
    context_menu::ContextMenu,
    icon::{Icon, IconKind},
    input::InputStateBuilder,
    interactive::InteractiveVariantExt,
    layer::LayerExt as _,
    menu::{Menu, MenuItem, MenuItemAction},
    scroll_bar::Scrollbar,
    separator::Separator,
    size::UnifiedSizable as _,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, ElementId, Entity, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
    base::{Disableable, Easing, Selectable, Transition, transition},
    div, px, uniform_list,
};
use i18n::t;
use smol_str::{SmolStr, format_smolstr};

use super::layout::{
    SIDEBAR_FULL_PADDING, SIDEBAR_ICON_SIZE, SIDEBAR_ITEM_GAP, SIDEBAR_ITEM_SIZE,
    SIDEBAR_NAME_MAX_WIDTH, SIDEBAR_PADDING,
};
use crate::windows::root::components::workspace::Workspaces;

/// The button above the list.
pub(crate) const SIDEBAR_NEW_WORKSPACE: &str = "SIDEBAR_NEW_WORKSPACE";

/// The label drawn beside the button above the list.
pub(crate) const SIDEBAR_NEW_WORKSPACE_LABEL: &str = "SIDEBAR_NEW_WORKSPACE_LABEL";

/// The divider drawn between the button above the list and the list itself.
const SIDEBAR_WORKSPACE_SEPARATOR: &str = "SIDEBAR_WORKSPACE_SEPARATOR";

/// The prefix of one workspace row's selector.
///
/// The rows are siblings, so each of them is named by the workspace it stands for rather
/// than by a name they all share.
const SIDEBAR_ITEM: &str = "SIDEBAR_ITEM";

/// The name drawn inside a row, which is absent while the sidebar draws icons only.
pub(crate) const SIDEBAR_WORKSPACE_NAME: &str = "SIDEBAR_WORKSPACE_NAME";

/// The workspace mark drawn inside a row, named so tests can say where it sits.
pub(crate) const SIDEBAR_WORKSPACE_MARK: &str = "SIDEBAR_WORKSPACE_MARK";

/// The prefix of one workspace row's close button selector.
const SIDEBAR_WORKSPACE_CLOSE: &str = "SIDEBAR_WORKSPACE_CLOSE";

/// The context menu a row's right click opens, named for tests.
const SIDEBAR_WORKSPACE_MENU_PANEL: &str = "SIDEBAR_WORKSPACE_MENU_PANEL";

/// How wide the workspace menu is.
///
/// The entries are small, so the two labels and their padding are all the width the menu
/// needs. The menu is opened at the pointer rather than against the row, so its width does
/// not move it away from where the user right-clicked.
const WORKSPACE_MENU_WIDTH: Pixels = px(120.0);

/// The item ids of the menu a row's right click opens.
const RENAME_ITEM: &str = "rename";
const DELETE_ITEM: &str = "delete";

/// The workspace section of the sidebar: add, then the list of workspaces.
#[derive(IntoElement)]
pub(crate) struct SidebarWorkspaces {
    /// Whether it draws the labelled panel rather than the icon-only rail.
    full: bool,
    /// Whether a selected or hovered row offers its close button.
    ///
    /// The panel turns this off while it retracts: the rows it is still drawing belong to a
    /// panel on its way out, and the control that belongs to the settled panel is not
    /// offered there.
    show_close: bool,
    workspaces: Entity<Workspaces>,
    /// Identifies one opening of the panel, so a row's ids and menus do not outlive it.
    popup_scope: u64,
}

impl SidebarWorkspaces {
    pub(crate) fn new(full: bool, workspaces: Entity<Workspaces>) -> Self {
        Self {
            full,
            show_close: true,
            workspaces,
            popup_scope: 0,
        }
    }

    /// Whether a selected or hovered row may draw its close button.
    pub(crate) fn show_close(mut self, show_close: bool) -> Self {
        self.show_close = show_close;
        self
    }

    /// Names this opening of the panel in the ids of its rows and menus.
    pub(crate) fn popup_scope(mut self, scope: u64) -> Self {
        self.popup_scope = scope;
        self
    }

    /// One workspace of the list: the row that selects it, the right click that renames or
    /// deletes it, and the close button the full panel draws while the row is selected or
    /// under the pointer.
    ///
    /// The row is a button in both panel states. A left click selects the workspace; a
    /// right click opens its menu at the pointer, and the row keeps its hover paint while
    /// the menu is up. The menu belongs to the window layer rather than to the row, so it
    /// is drawn over everything and outlives the row's own redraw; the icon-only rail opens
    /// the same menu as the full panel.
    fn row(
        full: bool,
        workspaces: &Entity<Workspaces>,
        popup_scope: u64,
        name: &SmolStr,
        active: bool,
        can_delete: bool,
        close_available: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let label = SharedString::from(name.as_str());
        let row_selector = format_smolstr!("{SIDEBAR_ITEM}_{name}");
        // The element ids namespace the name rather than concatenating the prefix into it:
        // a workspace named `ITEM_<other>` would otherwise give its row the id of
        // `<other>`'s row. The panel's opening is one more namespace, so the state of the
        // docked rail's rows is not shared with the floating panel drawn over it.
        let row_id: ElementId = (
            ElementId::NamedInteger(SharedString::new_static(SIDEBAR_ITEM), popup_scope),
            name.as_str(),
        )
            .into();
        let menu_id: ElementId = (row_id.clone(), "menu").into();
        // The menu's sheet covers the row, so the row's own hover ends when the menu opens.
        // This keeps the hover paint on the row for as long as its menu is up.
        let menu_open_state =
            window.use_keyed_state((row_id.clone(), "menu-open"), cx, |_, _| false);
        let menu_open = *menu_open_state.read(cx);

        let mut select = Button::new((row_id.clone(), "select"))
            .ghost()
            .selected(active)
            .force_hover(menu_open)
            .h(SIDEBAR_ITEM_SIZE)
            .debug_selector(move || row_selector.to_string())
            .aria_label(label.clone())
            .on_click({
                let workspaces = workspaces.clone();
                let name = name.clone();
                move |_, _, cx| {
                    workspaces.update(cx, |workspaces, cx| workspaces.select(&name, cx));
                }
            })
            .on_aux_click({
                let workspaces = workspaces.clone();
                let name = name.clone();
                move |event, window, cx| {
                    // `on_aux_click` covers every non-primary button; the workspace menu is
                    // the right button's.
                    if !event.is_right_click() {
                        return;
                    }

                    let entries = {
                        let menu_id = menu_id.clone();
                        move |_: &mut Window, cx: &mut App| {
                            Menu::new(menu_id.clone())
                                .small()
                                .item(MenuItem::Action(MenuItemAction::new(
                                    RENAME_ITEM,
                                    t!(cx, "window.sidebar.rename"),
                                )))
                                .item(MenuItem::Action(
                                    MenuItemAction::new(
                                        DELETE_ITEM,
                                        t!(cx, "window.sidebar.delete"),
                                    )
                                    .disabled(!can_delete),
                                ))
                        }
                    };
                    let on_item_click = {
                        let workspaces = workspaces.clone();
                        let name = name.clone();
                        move |_: &ClickEvent,
                              item_id: SharedString,
                              window: &mut Window,
                              cx: &mut App| match item_id.as_ref() {
                            RENAME_ITEM => show_rename_alert(&workspaces, &name, window, cx),
                            DELETE_ITEM => show_delete_alert(&workspaces, &name, window, cx),
                            unknown => {
                                log::error!("the workspace menu has no `{unknown}` entry")
                            }
                        }
                    };
                    let on_close = {
                        let menu_open_state = menu_open_state.clone();
                        move |cx: &mut App| menu_open_state.write(cx, false)
                    };
                    let context_id: ElementId = (menu_id.clone(), "context-menu").into();
                    // The label is read here rather than with the row: it is only needed for
                    // the menu a right click opens.
                    let menu_label = t!(cx, "window.sidebar.menu");

                    window.context_menu(
                        cx,
                        ContextMenu::new(
                            context_id,
                            event.position(),
                            WORKSPACE_MENU_WIDTH,
                            entries,
                        )
                        .aria_label(menu_label)
                        .debug_selector(SIDEBAR_WORKSPACE_MENU_PANEL)
                        .on_item_click(on_item_click)
                        .on_close(on_close),
                    );
                    // The opening is recorded after the layer has taken the menu: a menu
                    // this one replaces reports its close while the layer opens the new
                    // one, and that close must not turn the new menu's hover off.
                    menu_open_state.write(cx, true);
                }
            });
        let mark = div()
            .debug_selector(|| SIDEBAR_WORKSPACE_MARK.into())
            .child(Icon::new(IconKind::SquaresFour).size(SIDEBAR_ICON_SIZE));
        if !full {
            // The item is the toggle's square, so the column's marks line up with the
            // toggle in the title bar. The name is read from the full panel the pointer
            // floats over the rail, not from a tooltip.
            return select
                .w(SIDEBAR_ITEM_SIZE)
                .justify_center()
                .child(mark)
                .into_any_element();
        }

        select = select
            .w_full()
            .px(SIDEBAR_FULL_PADDING)
            .gap_2()
            .child(mark)
            .child(
                div()
                    .debug_selector(|| SIDEBAR_WORKSPACE_NAME.into())
                    .max_w(SIDEBAR_NAME_MAX_WIDTH)
                    // The panel narrows while it collapses: the name gives way to the
                    // mark rather than reaching past the panel's edge.
                    .min_w_0()
                    .truncate()
                    .child(label),
            );

        // The close button belongs to the row the page shows or the pointer is on. The
        // wrapper watches the row rather than the button itself: the button is not what
        // keeps itself up.
        let hover_state = window.use_keyed_state((row_id.clone(), "row-hover"), cx, |_, _| false);
        let show_close = close_available && can_delete && (active || *hover_state.read(cx));
        // The button appears and disappears with the row's own hover paint rather than
        // cutting in: both follow the same `ANIMATION_TICK` transition, so the fade and the
        // row's background settle together. The opacity is sampled even when the button is
        // not drawn, which is what carries a fade out to its end. A panel on its way out
        // takes the button away at once: the control belongs to the settled panel.
        let close_opacity: f32 = transition(
            (row_id.clone(), "close"),
            if show_close { 1.0 } else { 0.0 },
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );
        let mut row = div()
            .id((row_id.clone(), "row"))
            .relative()
            .w_full()
            .on_hover(move |hovered, _, cx| hover_state.write(cx, *hovered))
            .child(select);
        if close_available && (show_close || close_opacity > 0.0) {
            let close_selector = format_smolstr!("{SIDEBAR_WORKSPACE_CLOSE}_{name}");
            let close_label = t!(cx, "window.workspace.close");
            let close = Button::new((row_id, "close"))
                .ghost()
                .small()
                .absolute()
                .right(px(5.0))
                .top(px(6.0))
                .opacity(close_opacity)
                .debug_selector(move || close_selector.to_string())
                .aria_label(close_label)
                .child(Icon::new(IconKind::X).size(px(12.0)))
                // The row's select button lies under the close button, so a press here
                // must not reach it: selecting the workspace, or opening its menu, is not
                // what this button is for. The press never becomes the row's click, so
                // there is nothing to stop on the way back up.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_click({
                    let workspaces = workspaces.clone();
                    let name = name.clone();
                    move |_, window, cx| show_delete_alert(&workspaces, &name, window, cx)
                });
            row = row.child(close);
        }

        row.into_any_element()
    }
}

impl RenderOnce for SidebarWorkspaces {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let SidebarWorkspaces {
            full,
            show_close,
            workspaces,
            popup_scope,
        } = self;
        let (names, active, can_delete) = {
            let workspaces = workspaces.read(cx);
            (
                workspaces.names().cloned().collect::<Vec<_>>(),
                workspaces.active_name().clone(),
                workspaces.can_delete(),
            )
        };
        let add_label = t!(cx, "window.workspace.add");

        let mut add = Button::new(SIDEBAR_NEW_WORKSPACE)
            .ghost()
            .h(SIDEBAR_ITEM_SIZE)
            .debug_selector(|| SIDEBAR_NEW_WORKSPACE.into())
            .aria_label(add_label.clone())
            // The mark keeps its size while the panel narrows: a mark that shrinks would
            // slide into the label beside it on every frame of the movement.
            .child(
                Icon::new(IconKind::Plus)
                    .size(SIDEBAR_ICON_SIZE)
                    .flex_none(),
            )
            .on_click({
                let workspaces = workspaces.clone();
                move |_, window, cx| show_add_alert(&workspaces, window, cx)
            });
        add = if full {
            add.w_full().px(SIDEBAR_FULL_PADDING).gap_2().child(
                div()
                    .debug_selector(|| SIDEBAR_NEW_WORKSPACE_LABEL.into())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(add_label),
            )
        } else {
            add.w(SIDEBAR_ITEM_SIZE).justify_center()
        };

        // The rows are a uniform list so the panel can virtualise them and scroll them with
        // the scrollbar rather than the wheel alone. The handle is kept in the window's
        // keyed state: the list is rebuilt every frame, and its scroll position belongs to
        // the opening of the panel, not to one frame of its drawing.
        let scroll_state = window.use_keyed_state(
            ElementId::NamedInteger(
                SharedString::new_static("sidebar-workspaces-scroll"),
                popup_scope,
            ),
            cx,
            |_, _| UniformListScrollHandle::new(),
        );
        let scroll = scroll_state.read(cx).clone();
        let list = div()
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(
                uniform_list(
                    ElementId::NamedInteger(
                        SharedString::new_static("sidebar-workspaces"),
                        popup_scope,
                    ),
                    names.len(),
                    move |range, window, cx| {
                        range
                            .map(|index| {
                                let name = &names[index];
                                div()
                                    .w_full()
                                    .pb(SIDEBAR_ITEM_GAP)
                                    .child(SidebarWorkspaces::row(
                                        full,
                                        &workspaces,
                                        popup_scope,
                                        name,
                                        name == &active,
                                        can_delete,
                                        show_close,
                                        window,
                                        cx,
                                    ))
                                    .into_any_element()
                            })
                            .collect()
                    },
                )
                .track_scroll(&scroll)
                .size_full(),
            )
            .child(Scrollbar::vertical(scroll));

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(SIDEBAR_ITEM_GAP)
            .px(SIDEBAR_PADDING)
            .pb(SIDEBAR_PADDING)
            .child(add)
            .child(
                div()
                    .debug_selector(|| SIDEBAR_WORKSPACE_SEPARATOR.into())
                    .child(Separator::horizontal()),
            )
            .child(list)
    }
}

/// Builds the validator both naming alerts use.
///
/// An alert's validator runs without an application context, so the names a new name must
/// differ from are the ones that were taken when the alert opened. Nothing can add a
/// workspace while it is up: the alert is modal.
fn name_validator(
    taken: Vec<SmolStr>,
    cx: &App,
) -> impl Fn(&SharedString) -> Result<(), SharedString> + 'static {
    let required = t!(cx, "window.workspace.name_required");
    let exists = t!(cx, "window.workspace.name_exists");

    move |value| {
        let name = validate_workspace_name(value.as_str()).map_err(|_| required.clone())?;
        if taken.iter().any(|taken| taken.as_str() == name.as_str()) {
            return Err(exists.clone());
        }

        Ok(())
    }
}

/// Asks for the name of a new workspace, offering the next free default one.
fn show_add_alert(workspaces: &Entity<Workspaces>, window: &mut Window, cx: &mut App) {
    let title = t!(cx, "window.workspace.add_title");
    let message = t!(cx, "window.workspace.add_message");
    let placeholder = t!(cx, "window.workspace.name_placeholder");
    let confirm = t!(cx, "common.confirm");
    let cancel = t!(cx, "common.cancel");
    // The field starts at the next free default name, so adding a workspace without typing
    // anything does the obvious thing.
    let (suggested, taken) = {
        let workspaces = workspaces.read(cx);
        (
            workspaces.suggested_name(cx),
            workspaces.names().cloned().collect::<Vec<_>>(),
        )
    };
    let validate = name_validator(taken, cx);

    let workspaces = workspaces.clone();
    window.alert(cx, move |alert, window, cx| {
        let input = cx.new(|cx| {
            let mut state = InputStateBuilder::new("alert-input")
                .placeholder(placeholder)
                .build(window, cx);
            state.update_value(&suggested, window, cx);
            state
        });

        alert
            .input_ok_cancel(title, message, input)
            .ok_text(confirm)
            .cancel_text(cancel)
            .validate(validate)
            .ok_action(move |value, _, cx| {
                let value = value.unwrap_or_default();
                if let Err(error) =
                    workspaces.update(cx, |workspaces, cx| workspaces.add(&value, cx))
                {
                    log::error!("failed to add the workspace `{value}`: {error}");
                }
            })
    });
}

/// Asks for the new name of the workspace called `name`.
///
/// A workspace may keep the name it has, so it is not one of the names its own new name must
/// differ from.
fn show_rename_alert(
    workspaces: &Entity<Workspaces>,
    name: &SmolStr,
    window: &mut Window,
    cx: &mut App,
) {
    let title = t!(cx, "window.workspace.rename_title");
    let message = t!(cx, "window.workspace.rename_message", name = name.as_str());
    let placeholder = t!(cx, "window.workspace.name_placeholder");
    let confirm = t!(cx, "common.confirm");
    let cancel = t!(cx, "common.cancel");
    let taken = {
        let workspaces = workspaces.read(cx);
        workspaces
            .names()
            .filter(|taken| *taken != name)
            .cloned()
            .collect::<Vec<_>>()
    };
    let validate = name_validator(taken, cx);
    // The name the field starts at is also the workspace to rename: the alert is built after
    // this returns.
    let target = name.clone();

    let workspaces = workspaces.clone();
    window.alert(cx, move |alert, window, cx| {
        let input = cx.new(|cx| {
            let mut state = InputStateBuilder::new("alert-input")
                .placeholder(placeholder)
                .build(window, cx);
            state.update_value(&target, window, cx);
            state
        });

        alert
            .input_ok_cancel(title, message, input)
            .ok_text(confirm)
            .cancel_text(cancel)
            .validate(validate)
            .ok_action(move |value, _, cx| {
                let value = value.unwrap_or_default();
                if let Err(error) = workspaces.update(cx, |workspaces, cx| {
                    workspaces.rename_entry(&target, &value, cx)
                }) {
                    log::error!("failed to rename the workspace `{target}` to `{value}`: {error}");
                }
            })
    });
}

/// Asks whether the workspace called `name` should be deleted.
fn show_delete_alert(
    workspaces: &Entity<Workspaces>,
    name: &SmolStr,
    window: &mut Window,
    cx: &mut App,
) {
    let title = t!(cx, "window.workspace.delete_title");
    let message = t!(cx, "window.workspace.delete_message", name = name.as_str());
    let confirm = t!(cx, "common.confirm");
    let cancel = t!(cx, "common.cancel");
    let target = name.clone();

    let workspaces = workspaces.clone();
    window.alert(cx, move |alert, _, _| {
        alert
            .ok_cancel(title, message)
            .ok_text(confirm)
            .cancel_text(cancel)
            .ok_action(move |_, _, cx| {
                workspaces.update(cx, |workspaces, cx| workspaces.delete_entry(&target, cx));
            })
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use app_config::{
        settings::{Config, Language},
        ui::SidebarMode,
        workspace::{WorkspaceConfig, WorkspaceLayout, WorkspacesConfig},
    };
    use app_config_provider::manager::ConfigManager;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer, style};
    use gpui_kit::{
        AppContext, Context, Entity, IntoElement, Modifiers, MouseButton, Pixels, Render,
        ScrollDelta, ScrollEvent, Styled, TestAppContext, VisualTestContext, Window, div, point,
        px, size,
    };
    use i18n::language::codes::LanguageCode;

    use super::*;
    use crate::windows::root::components::sidebar::layout::{
        SIDEBAR_FULL_WIDTH, SIDEBAR_ICON_SIZE, SIDEBAR_ICON_WIDTH, SIDEBAR_ITEM_GAP,
        SIDEBAR_ITEM_SIZE, SIDEBAR_PADDING,
    };

    const PANEL: &str = "BASE_UI_ALERT";
    const OK: &str = "BASE_UI_ALERT_OK";
    const CANCEL: &str = "BASE_UI_ALERT_CANCEL";
    const INPUT: &str = "BASE_UI_INPUT";

    /// The close button of a row, named for the workspace it stands for.
    const CLOSE_ONE: &str = "SIDEBAR_WORKSPACE_CLOSE_one";
    const CLOSE_TWO: &str = "SIDEBAR_WORKSPACE_CLOSE_two";
    const CLOSE_NOTES: &str = "SIDEBAR_WORKSPACE_CLOSE_Notes";
    const CLOSE_ONLY: &str = "SIDEBAR_WORKSPACE_CLOSE_Only";

    /// The box the menu of a row's right click is drawn in. The panel belongs to the
    /// context menu, which names no selector of its own for it.
    const MENU_PANEL: &str = SIDEBAR_WORKSPACE_MENU_PANEL;
    const RENAME_ENTRY: &str = "TEST_MENU_ITEM_rename";
    const DELETE_ENTRY: &str = "TEST_MENU_ITEM_delete";

    /// The window a sidebar section is mounted in: the section at the width the panel gives
    /// it, over the layer every alert is opened on.
    struct Section {
        full: bool,
        width: Pixels,
        workspaces: Entity<Workspaces>,
    }

    impl Render for Section {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().child(
                div()
                    .w(self.width)
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(SidebarWorkspaces::new(self.full, self.workspaces.clone())),
            )
        }
    }

    /// Settings that already hold `names`, with the first one active, in English so a test
    /// can type a name the same way a user would.
    fn config_with(names: &[&str]) -> Config {
        let mut config = Config::default();
        config.language = Language::En;
        config.workspaces = WorkspacesConfig {
            entries: names
                .iter()
                .map(|name| WorkspaceConfig {
                    name: SmolStr::new(*name),
                    layout: WorkspaceLayout::single_pane(),
                })
                .collect(),
            active: names.first().map(|name| SmolStr::new(*name)),
        };

        config
    }

    /// Opens a window whose root is a layer, the way the application mounts one, with the
    /// section in the sidebar's own column width.
    fn section_window(
        cx: &mut TestAppContext,
        config: Config,
        mode: SidebarMode,
    ) -> (&mut VisualTestContext, Entity<Workspaces>) {
        let language = match config.language {
            Language::ZhCn => LanguageCode::ZhCn,
            Language::En => LanguageCode::En,
        };
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            widget_registry::catalogue::install(cx).expect("the catalogue should install");
            style::theme::install(cx);
            cx.set_global(language);
            let manager = ConfigManager::in_memory(config, cx);
            cx.set_global(manager);
        });
        let workspaces = cx.update(|cx| cx.new(Workspaces::load));
        let (full, width) = match mode {
            SidebarMode::Full => (true, SIDEBAR_FULL_WIDTH),
            SidebarMode::IconOnly => (false, SIDEBAR_ICON_WIDTH),
            SidebarMode::Hidden => panic!("a hidden sidebar is not drawn"),
        };
        let (_, cx) = cx.add_window_view({
            let workspaces = workspaces.clone();
            move |_, cx| {
                // The section reads the list while it draws, so it follows the store the way
                // the window around it does.
                let section = cx.new({
                    let workspaces = workspaces.clone();
                    move |cx| {
                        cx.observe(&workspaces, |_, _, cx| cx.notify()).detach();
                        Section {
                            full,
                            width,
                            workspaces,
                        }
                    }
                });

                Layer::new(section)
            }
        });
        cx.simulate_resize(size(px(400.0), px(600.0)));
        cx.run_until_parked();

        (cx, workspaces)
    }

    /// Lets the alert's own fade finish and repaints, so an alert in its first frames —
    /// painted at zero opacity — is not mistaken for one that stays.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        paint(cx);
    }

    /// Draws the frame the state just changed asks for, without moving the clock.
    fn paint(cx: &mut VisualTestContext) {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the sidebar should draw {selector}"))
            .center();
        cx.simulate_click(target, Modifiers::none());
        settle(cx);
    }

    /// Presses the right button at what `selector` names, and lets the menu it opens be
    /// drawn.
    fn right_click(cx: &mut VisualTestContext, selector: &'static str) {
        let target = drawn(cx, selector).center();
        right_click_at(cx, target);
    }

    /// Presses the right button at `at`, and lets the menu it opens be drawn.
    fn right_click_at(cx: &mut VisualTestContext, at: gpui_kit::Point<Pixels>) {
        cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
        settle(cx);
    }

    /// Moves the pointer to `at`, and draws the frame the move asks for.
    fn hover_at(cx: &mut VisualTestContext, at: gpui_kit::Point<Pixels>) {
        cx.simulate_mouse_move(at, None, Modifiers::none());
        settle(cx);
    }

    /// Moves the pointer to the middle of what `selector` names.
    fn hover(cx: &mut VisualTestContext, selector: &'static str) {
        let at = drawn(cx, selector).center();
        hover_at(cx, at);
    }

    /// Turns the wheel at `at`, and draws the frame the scroll asks for.
    fn wheel(cx: &mut VisualTestContext, at: gpui_kit::Point<Pixels>, lines: f32) {
        cx.simulate_mouse_move(at, None, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_event(ScrollEvent {
            position: at,
            delta: ScrollDelta::Lines(point(0.0, lines)),
            ..Default::default()
        });
        settle(cx);
    }

    /// Types into the alert's field, which is not focused until it is clicked.
    fn type_into_alert(cx: &mut VisualTestContext, text: &str) {
        click(cx, INPUT);
        cx.simulate_input(text);
        cx.run_until_parked();
    }

    fn names(cx: &VisualTestContext, workspaces: &Entity<Workspaces>) -> Vec<SmolStr> {
        workspaces.read_with(cx, |workspaces, _| workspaces.names().cloned().collect())
    }

    fn active_name(cx: &VisualTestContext, workspaces: &Entity<Workspaces>) -> SmolStr {
        workspaces.read_with(cx, |workspaces, _| workspaces.active_name().clone())
    }

    fn drawn(cx: &mut VisualTestContext, selector: &'static str) -> gpui_kit::Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("the sidebar should draw {selector}"))
    }

    /// Every workspace is a row of the list, one under the other, and the full sidebar names
    /// each of them beside its mark.
    #[gpui_kit::test]
    fn the_full_section_should_draw_a_named_row_for_every_workspace(cx: &mut TestAppContext) {
        let (cx, _) = section_window(cx, config_with(&["one", "two"]), SidebarMode::Full);

        assert!(
            cx.debug_bounds(SIDEBAR_NEW_WORKSPACE).is_some(),
            "the button that adds a workspace is above the list"
        );
        let first = drawn(cx, "SIDEBAR_ITEM_one");
        let second = drawn(cx, "SIDEBAR_ITEM_two");
        assert_eq!(
            first.size,
            size(
                SIDEBAR_FULL_WIDTH - SIDEBAR_PADDING * 2.0,
                SIDEBAR_ITEM_SIZE
            ),
            "a row takes the width the panel leaves it and one item's height"
        );
        assert_eq!(
            second.origin.y - first.origin.y,
            SIDEBAR_ITEM_SIZE + SIDEBAR_ITEM_GAP,
            "the rows are the list: one item, then the gap to the next one"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_WORKSPACE_NAME).is_some(),
            "the full sidebar draws the name of a workspace"
        );
        let add = drawn(cx, SIDEBAR_NEW_WORKSPACE);
        let divider = drawn(cx, SIDEBAR_WORKSPACE_SEPARATOR);
        assert!(
            add.bottom() < divider.top(),
            "the divider sits below the button that adds a workspace"
        );
        assert!(
            divider.bottom() < first.top(),
            "the list starts below the divider"
        );
        assert_eq!(
            divider.size.height,
            px(1.0),
            "the divider is the one-pixel line the separator draws"
        );
    }

    /// The list is a uniform list, not the panel's whole content: a list longer than the
    /// panel scrolls a window of rows, so the rows outside it are not drawn until they are
    /// scrolled into view.
    #[gpui_kit::test]
    fn the_workspace_list_should_scroll_its_rows(cx: &mut TestAppContext) {
        let names = (0..30).map(|index| format!("w{index}")).collect::<Vec<_>>();
        let configured = names.iter().map(String::as_str).collect::<Vec<_>>();
        let (cx, _) = section_window(cx, config_with(&configured), SidebarMode::Full);

        assert!(
            cx.debug_bounds("SIDEBAR_ITEM_w0").is_some(),
            "the list starts at its first row"
        );
        assert!(
            cx.debug_bounds("SIDEBAR_ITEM_w29").is_none(),
            "a row past the viewport is not drawn"
        );

        let at = drawn(cx, "SIDEBAR_ITEM_w1").center();
        wheel(cx, at, -100.0);

        assert!(
            cx.debug_bounds("SIDEBAR_ITEM_w0").is_none(),
            "the wheel moves the first row out of the viewport"
        );
        assert!(
            cx.debug_bounds("SIDEBAR_ITEM_w29").is_some(),
            "the last row is drawn once the wheel brings it into the viewport"
        );

        let at = drawn(cx, "SIDEBAR_ITEM_w29").center();
        wheel(cx, at, 100.0);

        assert!(
            cx.debug_bounds("SIDEBAR_ITEM_w0").is_some(),
            "the wheel scrolls the list back to its first row"
        );
    }

    /// The icon-only section keeps a row per workspace and drops the names, so a row is the
    /// title bar toggle's square and the rail is that square with one padding on either side.
    #[gpui_kit::test]
    fn the_icon_only_section_should_draw_its_rows_without_names(cx: &mut TestAppContext) {
        let (cx, _) = section_window(cx, config_with(&["one", "two"]), SidebarMode::IconOnly);

        assert!(
            cx.debug_bounds(SIDEBAR_NEW_WORKSPACE).is_some(),
            "adding a workspace is offered in both drawn states"
        );
        assert_eq!(
            drawn(cx, "SIDEBAR_ITEM_one").size,
            size(SIDEBAR_ITEM_SIZE, SIDEBAR_ITEM_SIZE),
            "an icon-only item is the square the title bar's toggle draws"
        );
        let add = drawn(cx, SIDEBAR_NEW_WORKSPACE);
        assert_eq!(
            add.size,
            size(SIDEBAR_ITEM_SIZE, SIDEBAR_ITEM_SIZE),
            "the button above the list is the same square"
        );
        assert_eq!(
            add.left(),
            SIDEBAR_PADDING,
            "the button starts at the padding, like the items below it"
        );
        let item = drawn(cx, "SIDEBAR_ITEM_one");
        assert_eq!(
            drawn(cx, SIDEBAR_WORKSPACE_MARK).left(),
            item.left() + (SIDEBAR_ITEM_SIZE - SIDEBAR_ICON_SIZE) / 2.0,
            "the mark is centred in the item's square, where the full row keeps it"
        );
        assert_eq!(
            item.left(),
            SIDEBAR_PADDING,
            "the item starts at the padding, under the title bar's toggle"
        );
        assert_eq!(
            item.right(),
            SIDEBAR_ICON_WIDTH - SIDEBAR_PADDING,
            "the rail leaves the same padding on the item's right"
        );
        assert!(
            cx.debug_bounds("SIDEBAR_ITEM_two").is_some(),
            "an icon-only sidebar still lists every workspace"
        );
        assert!(
            cx.debug_bounds(SIDEBAR_WORKSPACE_NAME).is_none(),
            "an icon-only sidebar draws no workspace name"
        );
    }

    /// The full panel draws the close button on the row the page shows and on the row under
    /// the pointer, on the row's right edge and centred in its height. A row that is neither
    /// selected nor hovered has none, and the button goes away when the pointer leaves.
    #[gpui_kit::test]
    fn the_close_button_should_follow_the_selected_and_hovered_rows(cx: &mut TestAppContext) {
        let (cx, _) = section_window(cx, config_with(&["one", "two"]), SidebarMode::Full);

        let row = drawn(cx, "SIDEBAR_ITEM_one");
        let close = drawn(cx, CLOSE_ONE);
        assert!(
            close.left() > row.center().x,
            "the close button belongs on the row's right: {close:?} in {row:?}"
        );
        assert_eq!(
            close.center().y,
            row.center().y,
            "the close button is centred in the row's height"
        );
        assert!(
            close.right() <= row.right(),
            "the close button stays inside the row it belongs to"
        );
        assert!(
            cx.debug_bounds(CLOSE_TWO).is_none(),
            "a row that is neither selected nor hovered draws no close button"
        );

        hover(cx, "SIDEBAR_ITEM_two");

        assert!(
            cx.debug_bounds(CLOSE_TWO).is_some(),
            "hovering a row draws its close button"
        );
        assert!(
            cx.debug_bounds(CLOSE_ONE).is_some(),
            "the selected row keeps its close button while another row is hovered"
        );

        hover(cx, CLOSE_TWO);

        assert!(
            cx.debug_bounds(CLOSE_TWO).is_some(),
            "the close button stays drawn while the pointer is on it"
        );

        hover_at(cx, point(px(350.0), px(550.0)));

        assert!(
            cx.debug_bounds(CLOSE_TWO).is_none(),
            "the close button goes away when the pointer leaves the row"
        );
    }

    /// The close button fades in and out with the row's own hover paint rather than cutting
    /// in and out: the row's background settles over one `ANIMATION_TICK`, and the button is
    /// drawn for as long as its own fade still has something to paint.
    #[gpui_kit::test]
    fn the_close_button_should_fade_out_rather_than_disappear_with_the_hover_state(
        cx: &mut TestAppContext,
    ) {
        let (cx, _) = section_window(cx, config_with(&["one", "two"]), SidebarMode::Full);

        hover(cx, "SIDEBAR_ITEM_two");
        settle(cx);
        assert!(
            cx.debug_bounds(CLOSE_TWO).is_some(),
            "the hovered row offers its close button"
        );

        // The pointer leaves the row. The row starts painting its hover away on this frame,
        // and the button starts its own fade instead of vanishing with the hover state.
        cx.simulate_mouse_move(point(px(350.0), px(550.0)), None, Modifiers::none());
        paint(cx);
        assert!(
            cx.debug_bounds(CLOSE_TWO).is_some(),
            "the close button is still drawn on the first frame of its fade-out"
        );

        // Halfway through the tick there is still a fade to paint; once the tick is up the
        // button has nothing left to draw and leaves the tree.
        cx.executor().advance_clock(ANIMATION_TICK / 2);
        paint(cx);
        assert!(
            cx.debug_bounds(CLOSE_TWO).is_some(),
            "the close button is drawn through its fade-out"
        );

        cx.executor().advance_clock(ANIMATION_TICK);
        paint(cx);
        assert!(
            cx.debug_bounds(CLOSE_TWO).is_none(),
            "the close button goes away once its fade-out has finished"
        );
    }

    /// Pressing the close button asks for the deletion the row menu asks for, and is not the
    /// row's selection: the workspace goes only after the confirmation, and cancelling it
    /// leaves both the list and the page where they were.
    #[gpui_kit::test]
    fn the_close_button_should_delete_the_workspace_after_the_rows_confirmation(
        cx: &mut TestAppContext,
    ) {
        let (cx, workspaces) =
            section_window(cx, config_with(&["Keep", "Notes"]), SidebarMode::Full);

        hover(cx, "SIDEBAR_ITEM_Notes");
        click(cx, CLOSE_NOTES);

        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "closing a workspace asks for the same confirmation the row menu does"
        );
        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("Keep"),
            "the press on the close button is not the row's selection"
        );
        assert_eq!(
            names(cx, &workspaces),
            [SmolStr::new_static("Keep"), SmolStr::new_static("Notes")],
            "nothing is deleted before the confirmation"
        );

        click(cx, CANCEL);

        assert_eq!(
            names(cx, &workspaces),
            [SmolStr::new_static("Keep"), SmolStr::new_static("Notes")],
            "cancelling leaves the workspace in the list"
        );
        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("Keep"),
            "cancelling leaves the page on the selected workspace"
        );

        hover(cx, "SIDEBAR_ITEM_Notes");
        click(cx, CLOSE_NOTES);
        click(cx, OK);

        assert_eq!(names(cx, &workspaces), [SmolStr::new_static("Keep")]);
        assert_eq!(active_name(cx, &workspaces), SmolStr::new_static("Keep"));
        assert!(
            cx.debug_bounds(CLOSE_NOTES).is_none(),
            "the deleted row takes its close button with it"
        );
    }

    /// The close button's area is not the row's right click: the workspace menu stays closed
    /// there, for the row the page shows and for the row under the pointer alike.
    #[gpui_kit::test]
    fn the_close_button_should_not_respond_to_a_right_click_as_the_row_does(
        cx: &mut TestAppContext,
    ) {
        let (cx, workspaces) = section_window(cx, config_with(&["one", "two"]), SidebarMode::Full);

        right_click(cx, CLOSE_ONE);
        assert!(
            cx.debug_bounds(MENU_PANEL).is_none(),
            "a right click on the selected row's close button opens no menu"
        );

        hover(cx, "SIDEBAR_ITEM_two");
        right_click(cx, CLOSE_TWO);
        assert!(
            cx.debug_bounds(MENU_PANEL).is_none(),
            "a right click on a hovered row's close button opens no menu"
        );
        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("one"),
            "the right click on the close button selected no workspace"
        );
        assert_eq!(
            names(cx, &workspaces),
            [SmolStr::new_static("one"), SmolStr::new_static("two")],
            "the right click on the close button deleted no workspace"
        );
    }

    /// The last workspace cannot be deleted, so its row offers no close button even while it
    /// is hovered.
    #[gpui_kit::test]
    fn the_last_workspace_should_not_draw_a_close_button(cx: &mut TestAppContext) {
        let (cx, _) = section_window(cx, config_with(&["Only"]), SidebarMode::Full);

        assert!(
            cx.debug_bounds(CLOSE_ONLY).is_none(),
            "the only workspace cannot be deleted"
        );
        hover(cx, "SIDEBAR_ITEM_Only");
        assert!(
            cx.debug_bounds(CLOSE_ONLY).is_none(),
            "hovering the only workspace still offers no close button"
        );
    }

    /// The icon-only rail has no close button at all: its row is one icon square, with no
    /// room beside the workspace mark.
    #[gpui_kit::test]
    fn the_icon_only_rail_should_not_draw_a_close_button(cx: &mut TestAppContext) {
        let (cx, _) = section_window(cx, config_with(&["one", "two"]), SidebarMode::IconOnly);

        hover(cx, "SIDEBAR_ITEM_one");
        assert!(
            cx.debug_bounds(CLOSE_ONE).is_none(),
            "the icon-only rail draws no close button on the selected row"
        );
        hover(cx, "SIDEBAR_ITEM_two");
        assert!(
            cx.debug_bounds(CLOSE_TWO).is_none(),
            "the icon-only rail draws no close button on a hovered row"
        );
    }

    /// Choosing a row with the left button switches to the workspace it names, and is not
    /// the button that opens its menu.
    #[gpui_kit::test]
    fn a_row_should_switch_to_the_workspace_it_names(cx: &mut TestAppContext) {
        let (cx, workspaces) = section_window(cx, config_with(&["one", "two"]), SidebarMode::Full);

        click(cx, "SIDEBAR_ITEM_two");

        assert_eq!(active_name(cx, &workspaces), SmolStr::new_static("two"));
        assert!(
            cx.debug_bounds(MENU_PANEL).is_none(),
            "a left click selects the workspace and opens no menu"
        );
    }

    /// A right click opens the row's menu at the pointer, at the menu's small size, and is
    /// not the row's selection.
    #[gpui_kit::test]
    fn a_right_click_should_open_the_rows_menu_at_the_pointer(cx: &mut TestAppContext) {
        let (cx, workspaces) = section_window(cx, config_with(&["one", "two"]), SidebarMode::Full);

        let row = drawn(cx, "SIDEBAR_ITEM_two");
        let at = point(row.left() + px(20.0), row.bottom() - px(4.0));
        right_click_at(cx, at);

        let panel = drawn(cx, MENU_PANEL);
        assert_eq!(
            panel.origin, at,
            "the menu's top-left corner sits where the right click was"
        );
        assert_eq!(
            drawn(cx, RENAME_ENTRY).size.height,
            px(20.0),
            "the menu entries are drawn at the small size"
        );
        assert!(
            cx.debug_bounds(DELETE_ENTRY).is_some(),
            "the menu offers both the rename and the delete"
        );
        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("one"),
            "opening a row's menu is not choosing the row"
        );
    }

    /// The icon-only rail opens the same menu: renaming and deleting an item do not need
    /// the full panel, and the right click belongs to the item under the pointer.
    #[gpui_kit::test]
    fn a_right_click_on_an_icon_only_row_should_open_its_menu(cx: &mut TestAppContext) {
        let (cx, _) = section_window(cx, config_with(&["one", "two"]), SidebarMode::IconOnly);

        right_click(cx, "SIDEBAR_ITEM_two");

        assert!(
            cx.debug_bounds(MENU_PANEL).is_some(),
            "the rail's item opens the workspace menu"
        );
        assert!(cx.debug_bounds(RENAME_ENTRY).is_some());
        assert!(cx.debug_bounds(DELETE_ENTRY).is_some());
    }

    /// The button above the list asks for a name, offers the next free default one, and
    /// switches to the workspace it adds.
    #[gpui_kit::test]
    fn the_new_workspace_button_should_add_a_workspace_and_switch_to_it(cx: &mut TestAppContext) {
        let (cx, workspaces) = section_window(cx, config_with(&["one"]), SidebarMode::Full);

        click(cx, SIDEBAR_NEW_WORKSPACE);
        assert!(cx.debug_bounds(PANEL).is_some(), "adding asks for a name");

        click(cx, OK);

        assert_eq!(
            names(cx, &workspaces),
            [
                SmolStr::new_static("one"),
                SmolStr::new_static("Workspace 1")
            ],
            "the field starts at the next free default name"
        );
        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("Workspace 1"),
            "a new workspace is the one the page shows"
        );
    }

    /// A row's menu renames that workspace, and an inactive one keeps both the list's order
    /// and the page.
    #[gpui_kit::test]
    fn the_row_menu_should_rename_an_inactive_workspace(cx: &mut TestAppContext) {
        let (cx, workspaces) =
            section_window(cx, config_with(&["Keep", "Notes"]), SidebarMode::Full);

        right_click(cx, "SIDEBAR_ITEM_Notes");
        assert!(
            cx.debug_bounds(MENU_PANEL).is_some(),
            "the right click opens the row's menu"
        );
        click(cx, RENAME_ENTRY);
        assert!(cx.debug_bounds(PANEL).is_some(), "renaming asks for a name");

        type_into_alert(cx, "-2");
        click(cx, OK);

        assert_eq!(
            names(cx, &workspaces),
            [SmolStr::new_static("Keep"), SmolStr::new_static("Notes-2")],
            "the renamed workspace keeps its place in the list"
        );
        assert_eq!(
            active_name(cx, &workspaces),
            SmolStr::new_static("Keep"),
            "renaming another workspace does not switch to it"
        );
    }

    /// Renaming refuses a name another workspace already uses, and keeps the alert open so
    /// the user can correct it.
    #[gpui_kit::test]
    fn renaming_should_refuse_a_name_another_workspace_uses(cx: &mut TestAppContext) {
        // The name the field starts at plus what the test types is the other workspace's
        // name, which is the one collision a field that can only be appended to can state.
        let (cx, workspaces) =
            section_window(cx, config_with(&["NotesKeep", "Notes"]), SidebarMode::Full);

        right_click(cx, "SIDEBAR_ITEM_Notes");
        click(cx, RENAME_ENTRY);
        type_into_alert(cx, "Keep");
        click(cx, OK);

        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "a name another workspace uses leaves the alert open"
        );
        assert_eq!(
            names(cx, &workspaces),
            [
                SmolStr::new_static("NotesKeep"),
                SmolStr::new_static("Notes")
            ],
            "the refused value changed nothing"
        );
    }

    /// A row's menu deletes that workspace after asking, and the workspace the page shows
    /// stays the one it was.
    #[gpui_kit::test]
    fn the_row_menu_should_delete_an_inactive_workspace(cx: &mut TestAppContext) {
        let (cx, workspaces) =
            section_window(cx, config_with(&["Keep", "Notes"]), SidebarMode::Full);

        right_click(cx, "SIDEBAR_ITEM_Notes");
        click(cx, DELETE_ENTRY);
        assert!(
            cx.debug_bounds(PANEL).is_some(),
            "deleting a workspace asks first"
        );

        click(cx, OK);

        assert_eq!(names(cx, &workspaces), [SmolStr::new_static("Keep")]);
        assert_eq!(active_name(cx, &workspaces), SmolStr::new_static("Keep"));
    }

    /// The last workspace cannot be deleted, so its menu offers no deletion at all rather
    /// than a button that would refuse.
    #[gpui_kit::test]
    fn the_last_workspace_should_not_be_deleted_from_its_menu(cx: &mut TestAppContext) {
        let (cx, workspaces) = section_window(cx, config_with(&["Only"]), SidebarMode::Full);

        right_click(cx, "SIDEBAR_ITEM_Only");
        assert!(cx.debug_bounds(MENU_PANEL).is_some());
        click(cx, DELETE_ENTRY);

        assert!(
            cx.debug_bounds(PANEL).is_none(),
            "the only workspace cannot be deleted, so nothing asks to confirm it"
        );
        assert_eq!(names(cx, &workspaces), [SmolStr::new_static("Only")]);
    }

    #[test]
    fn a_name_validator_should_reject_empty_and_taken_names() {
        let cx = TestAppContext::single();

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            let required = t!(cx, "window.workspace.name_required");
            let exists = t!(cx, "window.workspace.name_exists");
            let validate = name_validator(vec![SmolStr::new_static("Taken")], cx);

            assert_eq!(validate(&SharedString::new_static("   ")), Err(required));
            assert_eq!(
                validate(&SharedString::new_static("  Taken  ")),
                Err(exists),
                "a padded name collides with the same name trimmed"
            );
            assert_eq!(validate(&SharedString::new_static("Taken 2")), Ok(()));
            assert_eq!(validate(&SharedString::new_static("taken")), Ok(()));
        });
    }
}

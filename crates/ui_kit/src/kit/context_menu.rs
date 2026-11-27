use std::{cell::RefCell, fmt, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, KeyBinding, KeyCode, Modifiers},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::id::NodeId,
    popup::{
        Alignment, Anchor, DismissReason, OutsidePress, PlacementSpec, PopupError, ShiftPolicy,
        Side,
    },
    window::WindowId,
};

use crate::kit::{
    menu::{Menu, MenuCheckState, MenuEntry, MenuOptions},
    size::UnifiedSize,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuAnchor {
    #[default]
    TopLeft,
    BottomLeft,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextMenuOptions {
    pub size: UnifiedSize,
    pub width: LogicalPx,
    pub max_height: LogicalPx,
    pub anchor: MenuAnchor,
    pub label: Option<SmolStr>,
    pub outside_press: OutsidePress,
}

impl Default for ContextMenuOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            width: px(200.0),
            max_height: px(320.0),
            anchor: MenuAnchor::TopLeft,
            label: None,
            outside_press: OutsidePress::Replay,
        }
    }
}

impl ContextMenuOptions {
    #[must_use]
    pub fn width(mut self, width: LogicalPx) -> Self {
        self.width = width;
        self
    }

    #[must_use]
    pub fn max_height(mut self, max_height: LogicalPx) -> Self {
        self.max_height = max_height;
        self
    }

    #[must_use]
    pub fn anchor(mut self, anchor: MenuAnchor) -> Self {
        self.anchor = anchor;
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn outside_press(mut self, outside_press: OutsidePress) -> Self {
        self.outside_press = outside_press;
        self
    }
}

#[derive(Clone)]
pub struct ContextMenu {
    inner: Rc<ContextMenuInner>,
}

struct ContextMenuInner {
    key: SmolStr,
    menu: RefCell<Menu>,
    on_item_click: RefCell<Option<Rc<dyn Fn(&mut App, SmolStr, Option<MenuCheckState>)>>>,
    trigger_action: RefCell<Option<Action>>,
}

impl ContextMenu {
    pub fn new(
        app: &mut App,
        theme: crate::theme::handle::ThemeHandle,
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        options: ContextMenuOptions,
    ) -> Self {
        let key = key.into();
        let menu = Menu::new(
            app,
            theme,
            format_smolstr!("{key}:menu"),
            entries,
            menu_options(&options),
        )
        .closing_on_scroll();
        let inner = Rc::new(ContextMenuInner {
            key,
            menu: RefCell::new(menu),
            on_item_click: RefCell::new(None),
            trigger_action: RefCell::new(None),
        });
        let context_menu = Self { inner };
        context_menu.install_handlers();
        context_menu
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner.menu.borrow().is_open(app)
    }

    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.menu.borrow().panel_node()
    }

    #[must_use]
    pub fn popup_node(&self) -> Option<NodeId> {
        self.inner.menu.borrow().popup_node()
    }

    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.menu.borrow().highlighted(app)
    }

    pub fn set_entries(
        &self,
        app: &mut App,
        entries: impl IntoIterator<Item = MenuEntry>,
    ) -> Result<(), EngineError> {
        self.inner.menu.borrow().set_entries(app, entries)
    }

    #[must_use]
    pub fn entries(&self) -> Rc<Vec<MenuEntry>> {
        self.inner.menu.borrow().entries()
    }

    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        point: LogicalPoint,
    ) -> Result<bool, PopupError> {
        self.open_at(app, window, point, None)
    }

    pub fn open_at(
        &self,
        app: &mut App,
        window: WindowId,
        point: LogicalPoint,
        highlighted: Option<&str>,
    ) -> Result<bool, PopupError> {
        let anchor = Anchor::WindowRect(LogicalRect::from_origin_size(point, LogicalSize::ZERO));
        self.inner
            .menu
            .borrow()
            .open_at(app, window, anchor, highlighted)
    }

    pub fn open_on(
        &self,
        app: &mut App,
        window: WindowId,
        node: NodeId,
    ) -> Result<bool, PopupError> {
        let bounds = app
            .window(window)
            .and_then(|target| target.node_screen_rect(node))
            .ok_or(PopupError::AnchorNodeMissing { node })?;
        let point = LogicalPoint::new(bounds.origin.x, bounds.max().y);
        self.open_at(app, window, point, None)
    }

    pub fn bind_trigger(
        &self,
        app: &mut App,
        window: WindowId,
        trigger: NodeId,
    ) -> Result<(), EngineError> {
        if let Some(previous) = self.inner.trigger_action.borrow_mut().take() {
            app.off_action(&previous);
        }
        let action = Action::Custom(format_smolstr!("{}.trigger", self.inner.key));
        let this = self.clone();
        app.on_action(action.clone(), move |ctx| {
            let node = ctx.node();
            let window = ctx.window();
            let app = ctx.app_mut();
            let _ = this.open_on(app, window, node);
        });
        self.inner
            .trigger_action
            .borrow_mut()
            .replace(action.clone());
        for (key, modifiers) in [
            (KeyCode::F10, Modifiers::SHIFT),
            (KeyCode::ContextMenu, Modifiers::empty()),
        ] {
            app.bind_key_scoped(
                window,
                trigger,
                KeyBinding {
                    key,
                    modifiers,
                    action: action.clone(),
                },
            )?;
        }
        Ok(())
    }

    pub fn close(&self, app: &mut App) -> bool {
        self.inner.menu.borrow().close(app)
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        self.inner.menu.borrow().close_now(app)
    }

    pub fn on_item_click(
        self,
        callback: impl Fn(&mut App, SmolStr, Option<MenuCheckState>) + 'static,
    ) -> Self {
        *self.inner.on_item_click.borrow_mut() = Some(Rc::new(callback));
        self
    }

    pub fn on_close(self, callback: impl Fn(&mut App, DismissReason) + 'static) -> Self {
        let menu = self.inner.menu.borrow().clone().on_close(callback);
        *self.inner.menu.borrow_mut() = menu;
        self
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        self.inner.menu.borrow().sync(app)
    }

    pub fn release(&self, app: &mut App) -> usize {
        if let Some(action) = self.inner.trigger_action.borrow_mut().take() {
            app.off_action(&action);
        }
        self.inner.menu.borrow().release(app)
    }

    fn install_handlers(&self) {
        let weak = Rc::downgrade(&self.inner);
        let menu = self
            .inner
            .menu
            .borrow()
            .clone()
            .on_activate(move |app, activation| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.menu.borrow().close_now(app);
                if let Some(callback) = inner.on_item_click.borrow().clone() {
                    callback(app, activation.id, activation.check);
                }
            });
        *self.inner.menu.borrow_mut() = menu;
    }
}

fn menu_options(options: &ContextMenuOptions) -> MenuOptions {
    let placement = match options.anchor {
        MenuAnchor::TopLeft => PlacementSpec {
            side: Side::Bottom,
            align: Alignment::Start,
            gap: LogicalPx::ZERO,
            shift: ShiftPolicy::None,
            ..PlacementSpec::default()
        },
        MenuAnchor::BottomLeft => PlacementSpec {
            side: Side::Top,
            align: Alignment::Start,
            gap: LogicalPx::ZERO,
            shift: ShiftPolicy::None,
            ..PlacementSpec::default()
        },
    };
    MenuOptions {
        size: options.size,
        width: options.width,
        max_height: options.max_height,
        placement,
        label: options.label.clone(),
        outside_press: options.outside_press,
        ..MenuOptions::default()
    }
}

impl fmt::Debug for ContextMenu {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ContextMenu").finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Duration,
    };

    use smol_str::SmolStr;
    use ui_core::{
        a11y::A11yRole,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        interaction::ScrollConfig,
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::style_builder::StyleBuilder,
        popup::{DismissReason, PopupError, PopupId, PopupSnapshotEntry, PopupState},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{ContextMenu, ContextMenuOptions, MenuAnchor, MenuEntry};
    use crate::{
        kit::{
            menu::{MenuAction, MenuCheck, MenuCheckState, MenuSubmenu},
            test_support::a11y_snapshot_without_oracle,
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        reported: Rc<RefCell<Vec<SmolStr>>>,
        closes: Rc<Cell<usize>>,
        background_clicks: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let clicks = Rc::new(Cell::new(0));
            let sink = clicks.clone();
            app.app_mut().on_action(
                Action::Custom(SmolStr::new_static("background.click")),
                move |_| sink.set(sink.get() + 1),
            );
            NodeBuilder::boxed()
                .id("background")
                .layout_override(LogicalRect::new(px(600.0), px(500.0), px(80.0), px(40.0)))
                .on_click(Action::Custom(SmolStr::new_static("background.click")))
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("背景节点");
            Self {
                app,
                window,
                theme,
                reported: Rc::new(RefCell::new(Vec::new())),
                closes: Rc::new(Cell::new(0)),
                background_clicks: clicks,
            }
        }

        fn menu(&mut self, options: ContextMenuOptions) -> ContextMenu {
            self.menu_with(entries(), options)
        }

        fn menu_with(
            &mut self,
            entries: Vec<MenuEntry>,
            options: ContextMenuOptions,
        ) -> ContextMenu {
            let reported = self.reported.clone();
            let closes = self.closes.clone();
            ContextMenu::new(
                self.app.app_mut(),
                self.theme.clone(),
                "context",
                entries,
                options,
            )
            .on_item_click(move |_, id, _| reported.borrow_mut().push(id))
            .on_close(move |_, _| closes.set(closes.get() + 1))
        }

        fn open(&mut self, menu: &ContextMenu, at: LogicalPoint) -> bool {
            let opened = menu
                .open(self.app.app_mut(), self.window, at)
                .expect("打开");
            self.settle(menu);
            opened
        }

        fn settle(&mut self, menu: &ContextMenu) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                let _ = self.app.drain().expect("帧");
                let _ = menu.sync(self.app.app_mut());
            }
            self.app.advance(Duration::from_millis(200));
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn drain(&mut self, menu: &ContextMenu) -> ui_core::test_support::record::FrameRecord {
            let record = self.app.drain().expect("帧").clone();
            let _ = menu.sync(self.app.app_mut());
            record
        }

        fn click(&mut self, menu: &ContextMenu, at: LogicalPoint) {
            let _ = self.app.inject(InputEvent::pointer_down(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            self.settle(menu);
        }

        fn key(&mut self, menu: &ContextMenu, code: KeyCode) {
            let _ = self
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
            self.settle(menu);
        }

        fn bounds(&self, node: NodeId) -> LogicalRect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn find(&self, debug_id: &str) -> Option<NodeId> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![tree.root()];
            while let Some(node) = stack.pop() {
                if tree.debug_id(node) == Some(debug_id) {
                    return Some(node);
                }
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            None
        }

        fn row_center(&self, index: usize) -> LogicalPoint {
            let node = self
                .find(&format!("context:menu:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"));
            let bounds = self.bounds(node);
            LogicalPoint::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn hover(&mut self, menu: &ContextMenu, at: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(at, Modifiers::empty()));
            self.settle(menu);
        }

        fn stack(&self) -> Vec<PopupSnapshotEntry> {
            self.app
                .app()
                .popup_stack(self.window)
                .expect("浮层栈")
                .entries
        }

        fn root_entry(&self, menu: &ContextMenu) -> PopupSnapshotEntry {
            let node = menu.popup_node().expect("浮层内容节点");
            self.stack()
                .into_iter()
                .find(|entry| entry.node == node)
                .expect("根菜单在栈里")
        }

        fn submenu_entry(&self, root: PopupId) -> PopupSnapshotEntry {
            self.stack()
                .into_iter()
                .find(|entry| entry.parent == Some(root))
                .expect("子菜单在栈里")
        }
    }

    fn entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("copy", "复制")),
            MenuEntry::Action(MenuAction::new("paste", "粘贴")),
        ]
    }

    fn options() -> ContextMenuOptions {
        ContextMenuOptions {
            width: px(200.0),
            ..ContextMenuOptions::default()
        }
    }

    fn entries_with_submenu() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("copy", "复制")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [MenuEntry::Action(MenuAction::new("inner", "内部"))],
            )),
        ]
    }

    #[test]
    fn the_menu_opens_with_its_top_left_corner_on_the_point() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        let point = LogicalPoint::new(px(300.0), px(200.0));
        assert!(harness.open(&menu, point));
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert_eq!(panel.origin, point, "TopLeft 锚点：面板左上角在点上");
        assert_eq!(panel.size.width.value(), 200.0, "面板宽度 = 构造参数");
    }

    #[test]
    fn a_bottom_left_menu_grows_up_from_its_point() {
        let mut harness = Harness::new();
        let menu = harness.menu(ContextMenuOptions {
            anchor: MenuAnchor::BottomLeft,
            ..options()
        });
        let point = LogicalPoint::new(px(300.0), px(300.0));
        assert!(harness.open(&menu, point));
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert_eq!(panel.origin.x, point.x, "左下角锚点：左边缘在点上");
        assert!(
            (panel.max().y - point.y).abs().value() < 0.5,
            "左下角锚点：底边在点上（实际 {panel:?} / 点 {point:?}）"
        );
    }

    #[test]
    fn the_first_visible_frame_already_has_the_settled_rect() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        let point = LogicalPoint::new(px(300.0), px(200.0));
        let opened = menu
            .open(harness.app.app_mut(), harness.window, point)
            .expect("打开");
        assert!(opened);
        let _ = menu.sync(harness.app.app_mut());
        harness.drain(&menu);
        let first_entry = harness.root_entry(&menu);
        assert_eq!(first_entry.state, PopupState::Open, "首帧即 Open");
        let first = first_entry.hit_rect;
        harness.settle(&menu);
        assert_eq!(
            first,
            harness.root_entry(&menu).hit_rect,
            "首帧 rect 等于稳定帧"
        );
    }

    #[test]
    fn the_menu_stays_inside_the_viewport_near_an_edge() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        assert!(harness.open(&menu, LogicalPoint::new(px(790.0), px(590.0))));
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.max().x.value() <= 800.0 - 4.0 + 0.5
                && panel.max().y.value() <= 600.0 - 4.0 + 0.5,
            "贴边时被 clamp 进视口（留 4 px 边距）：{panel:?}"
        );
    }

    #[test]
    fn choosing_an_entry_closes_the_menu_before_reporting_it() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        let at = harness.row_center(1);
        harness.click(&menu, at);

        assert_eq!(
            harness.reported.borrow().as_slice(),
            &[SmolStr::new_static("paste")],
            "条目上报"
        );
        assert!(!menu.is_open(harness.app.app()), "上报前已关菜单");
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_press_outside_closes_the_menu_and_replays_the_press_to_the_page() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        harness.click(&menu, LogicalPoint::new(px(640.0), px(520.0)));
        assert!(!menu.is_open(harness.app.app()), "外部按下关闭菜单");
        assert_eq!(
            harness.background_clicks.get(),
            1,
            "D14：Auto 根外部按下按 Replay 重派给下层命中控件"
        );
    }

    #[test]
    fn escape_closes_the_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()));
        assert_eq!(harness.closes.get(), 1);

        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        assert!(menu.close_now(harness.app.app_mut()));
        harness.settle(&menu);
        assert!(!menu.is_open(harness.app.app()));
        assert_eq!(harness.closes.get(), 2, "每次关闭回调一次");
    }

    #[test]
    fn a_second_context_menu_replaces_the_first() {
        let mut harness = Harness::new();
        let first = harness.menu(options());
        let second = harness.menu(options());
        harness.open(&first, LogicalPoint::new(px(300.0), px(200.0)));
        assert!(first.is_open(harness.app.app()));
        harness.open(&second, LogicalPoint::new(px(400.0), px(300.0)));
        assert!(
            !first.is_open(harness.app.app()),
            "Auto 根替换：后开的右键菜单关闭前一个"
        );
        assert!(second.is_open(harness.app.app()));
        assert_eq!(harness.stack().len(), 1);
    }

    #[test]
    fn reopening_closes_the_previous_submenu() {
        let mut harness = Harness::new();
        let menu = harness.menu_with(entries_with_submenu(), options());
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        let root_node = menu.popup_node().expect("根浮层节点");
        let root_id = harness.root_entry(&menu).id;
        let at = harness.row_center(1);
        harness.hover(&menu, at);
        assert!(
            harness
                .find("context:menu:sub:1:panel:list:row:0")
                .is_some(),
            "悬停打开子菜单"
        );
        assert_eq!(harness.stack().len(), 2, "根菜单 + 子菜单两层");

        harness.open(&menu, LogicalPoint::new(px(200.0), px(150.0)));
        assert!(
            harness
                .find("context:menu:sub:1:panel:list:row:0")
                .is_none(),
            "自身重开时旧子菜单被关掉"
        );
        assert_eq!(
            menu.popup_node(),
            Some(root_node),
            "自己的根被保留（只换锚点，不重建面板）"
        );
        assert_eq!(harness.root_entry(&menu).id, root_id, "根 PopupId 不变");
        assert_eq!(harness.stack().len(), 1, "栈只剩根菜单一层");
    }

    #[test]
    fn open_at_puts_the_highlight_on_the_named_entry() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        let opened = menu
            .open_at(
                harness.app.app_mut(),
                harness.window,
                LogicalPoint::new(px(300.0), px(200.0)),
                Some("paste"),
            )
            .expect("打开");
        assert!(opened);
        harness.settle(&menu);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
    }

    #[test]
    fn the_options_label_reaches_the_menu_panel() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(ContextMenuOptions {
            label: Some(SmolStr::new_static("编辑")),
            ..options()
        });
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        assert!(menu.is_open(harness.app.app()));
        let _ = harness.app.drain().expect("帧");
        harness.app.assert_a11y_contract();
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let wrapper = menu.popup_node().expect("浮层内容节点");
        let record = snapshot.node_for(wrapper).expect("包装节点在 a11y 树里");
        assert_eq!(record.role, A11yRole::Menu);
        assert_eq!(
            record
                .label
                .as_ref()
                .map(|label| label.text().to_owned())
                .as_deref(),
            Some("编辑"),
            "ContextMenuOptions::label 透传到浮层包装节点的可访问名"
        );
    }

    #[test]
    fn a_submenu_stacks_above_the_root_and_escape_closes_it_first() {
        let mut harness = Harness::new();
        let menu = harness.menu_with(entries_with_submenu(), options());
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        let root_id = harness.root_entry(&menu).id;
        let root_z = harness.root_entry(&menu).z;

        let at = harness.row_center(1);
        harness.hover(&menu, at);
        let submenu = harness.submenu_entry(root_id);
        assert!(submenu.z > root_z, "子菜单 z 在父之上");
        assert_eq!(submenu.parent, Some(root_id), "子菜单 parent 指向父菜单");

        harness.key(&menu, KeyCode::Escape);
        assert!(menu.is_open(harness.app.app()), "Esc 先关子菜单");
        assert_eq!(harness.stack().len(), 1, "子菜单已关闭，根菜单仍在栈里");
        assert!(
            harness
                .find("context:menu:sub:1:panel:list:row:0")
                .is_none(),
            "子菜单节点已卸载"
        );
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()), "第二次 Esc 关父菜单");
        assert_eq!(harness.closes.get(), 1, "根菜单只上报一次关闭");
    }

    fn scroll_page(harness: &mut Harness) -> ui_core::interaction::ScrollHandle {
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let container = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(px(800.0))
                        .height(px(600.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("页面")
        };
        let handle = harness
            .app
            .app_mut()
            .make_scroll(harness.window, container, ScrollConfig::default())
            .expect("滚动容器");
        harness
            .app
            .app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(800.0), px(2400.0)))
            .expect("内容");
        handle
    }

    fn wheel(harness: &mut Harness, menu: &ContextMenu, at: LogicalPoint) {
        let _ = harness.app.inject(InputEvent::scroll_lines(
            at,
            LogicalPoint::new(px(0.0), px(-2.0)),
            Modifiers::empty(),
        ));
        harness.settle(menu);
    }

    fn page_offset(harness: &Harness, handle: &ui_core::interaction::ScrollHandle) -> f32 {
        f32::from(
            harness
                .app
                .app()
                .scroll_state(handle)
                .expect("滚动状态")
                .offset
                .y,
        )
    }

    #[test]
    fn scrolling_the_page_closes_the_open_context_menu() {
        let mut harness = Harness::new();
        let page = scroll_page(&mut harness);
        let menu = harness.menu(options());
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));

        wheel(&mut harness, &menu, LogicalPoint::new(px(700.0), px(500.0)));
        assert!(
            page_offset(&harness, &page) > 50.0,
            "Auto 非模态右键菜单不吞背景滚动（旧 modal 行为已按设计变更）"
        );
        assert!(
            !menu.is_open(harness.app.app()),
            "背景滚动必须关闭右键菜单整链（设计 §4.7 矩阵）"
        );
        assert_eq!(harness.closes.get(), 1, "关闭只上报一次");
    }

    #[test]
    fn a_page_scroll_close_reports_anchor_lost() {
        let mut harness = Harness::new();
        let page = scroll_page(&mut harness);
        let reasons: Rc<RefCell<Vec<DismissReason>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = reasons.clone();
        let menu = harness
            .menu(options())
            .on_close(move |_, reason| sink.borrow_mut().push(reason));
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        assert!(menu.is_open(harness.app.app()), "右键菜单打开");

        wheel(&mut harness, &menu, LogicalPoint::new(px(700.0), px(500.0)));
        assert!(page_offset(&harness, &page) > 50.0);
        assert!(!menu.is_open(harness.app.app()));
        assert_eq!(
            reasons.borrow().as_slice(),
            &[DismissReason::AnchorLost],
            "外部滚动关闭整链必须上报 AnchorLost（R2-02/Q2-07）"
        );
    }

    #[test]
    fn opening_and_closing_the_context_menu_never_forces_a_full_repaint() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        let id = harness.root_entry(&menu).id;
        for _ in 0..4 {
            let record = harness.drain(&menu);
            assert!(!record.damage_full, "打开右键菜单不得全量重绘");
        }
        assert!(menu.close(harness.app.app_mut()));
        for _ in 0..12 {
            harness.app.advance(Duration::from_millis(16));
            let record = harness.drain(&menu);
            assert!(!record.damage_full, "关闭右键菜单不得全量重绘");
            if harness.app.app().popup_rect(id).is_none() {
                break;
            }
        }
        assert!(
            harness.app.app().popup_rect(id).is_none(),
            "关闭动画结束后节点卸载"
        );
        assert_eq!(harness.closes.get(), 1);
    }
    fn focusable_node(harness: &mut Harness, id: &'static str, rect: LogicalRect) -> NodeId {
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let action = Action::Custom(SmolStr::new_static("context.trigger.click"));
        harness.app.app_mut().on_action(action.clone(), move |_| {});
        NodeBuilder::boxed()
            .id(id)
            .tab_stop(true)
            .on_click(action)
            .layout_override(rect)
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(harness.window)
                    .expect("窗口")
                    .tree_mut(),
                root,
            )
            .expect("可聚焦节点")
    }

    #[test]
    fn shift_f10_on_the_focus_node_opens_the_menu_at_the_node() {
        let mut harness = Harness::new();
        let trigger = focusable_node(
            &mut harness,
            "context.trigger",
            LogicalRect::new(px(300.0), px(200.0), px(120.0), px(32.0)),
        );
        let _ = harness.app.drain().expect("布局帧");
        let menu = harness.menu(options());
        menu.bind_trigger(harness.app.app_mut(), harness.window, trigger)
            .expect("绑定 Shift+F10");
        assert!(
            harness
                .app
                .app_mut()
                .set_focus(harness.window, Some(trigger))
        );
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::F10, Modifiers::SHIFT));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::F10, Modifiers::SHIFT));
        harness.settle(&menu);
        assert!(menu.is_open(harness.app.app()), "Shift+F10 打开右键菜单");
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            (panel.origin.y - 232.0).abs().value() < 0.5,
            "面板锚在节点底边：{panel:?}"
        );

        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点回到触发节点"
        );

        let _ = harness.app.inject(InputEvent::key_down(
            KeyCode::ContextMenu,
            Modifiers::empty(),
        ));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::ContextMenu, Modifiers::empty()));
        harness.settle(&menu);
        assert!(
            menu.is_open(harness.app.app()),
            "上下文菜单键同样打开右键菜单"
        );
    }

    #[test]
    fn a_context_menu_chain_opens_three_levels_and_closes_children_first() {
        let mut harness = Harness::new();
        let menu = harness.menu_with(
            vec![
                MenuEntry::Action(MenuAction::new("copy", "复制")),
                MenuEntry::Submenu(MenuSubmenu::new(
                    "更多",
                    [
                        MenuEntry::Action(MenuAction::new("inner", "内部")),
                        MenuEntry::Submenu(MenuSubmenu::new(
                            "更深",
                            [MenuEntry::Action(MenuAction::new("deep", "深处"))],
                        )),
                    ],
                )),
            ],
            options(),
        );
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        let root = harness.root_entry(&menu).id;
        let level2 = {
            let handle = menu.inner.menu.borrow().clone();
            handle
                .open_submenu(harness.app.app_mut(), 1)
                .expect("二级")
                .expect("二级句柄")
        };
        let level3 = level2
            .open_submenu(harness.app.app_mut(), 1)
            .expect("三级")
            .expect("三级句柄");
        harness.settle(&menu);
        assert_eq!(harness.stack().len(), 3, "右键菜单 3 级链");
        let entries = harness.stack();
        assert!(entries[0].z < entries[1].z && entries[1].z < entries[2].z);
        assert_eq!(entries[1].parent, Some(root));
        assert_eq!(entries[2].parent, Some(entries[1].id));
        assert!(
            level3.popup_node() != level2.popup_node(),
            "三级与二级是两个独立面板"
        );

        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert_eq!(harness.stack().len(), 2, "Esc 逐层：先关三级");
        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert_eq!(harness.stack().len(), 1);
        assert!(menu.is_open(harness.app.app()));
    }

    #[test]
    fn a_consuming_context_menu_swallows_the_outside_press() {
        let mut harness = Harness::new();
        let menu = harness.menu(ContextMenuOptions {
            outside_press: ui_core::popup::OutsidePress::Consume,
            ..options()
        });
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        harness.click(&menu, LogicalPoint::new(px(640.0), px(520.0)));
        assert!(!menu.is_open(harness.app.app()), "外部按下关闭菜单");
        assert_eq!(
            harness.background_clicks.get(),
            0,
            "显式 Consume 不再把按下 replay 给背景"
        );
    }

    #[test]
    fn opening_on_a_removed_node_reports_the_missing_anchor() {
        let mut harness = Harness::new();
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let gone = NodeBuilder::boxed()
            .id("context-gone")
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(1.0), px(1.0)))
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(harness.window)
                    .expect("窗口")
                    .tree_mut(),
                root,
            )
            .expect("临时节点");
        harness
            .app
            .app_mut()
            .window_mut(harness.window)
            .expect("窗口")
            .tree_mut()
            .remove(gone)
            .expect("移除临时节点");
        let menu = harness.menu_with(
            vec![MenuEntry::Action(MenuAction::new("copy", "复制"))],
            options(),
        );
        let error = menu
            .open_on(harness.app.app_mut(), harness.window, gone)
            .expect_err("缺失节点必须返回强类型错误");
        assert!(matches!(
            error,
            PopupError::AnchorNodeMissing { node } if node == gone
        ));
    }

    #[test]
    fn binding_the_trigger_twice_keeps_a_single_keyboard_handler() {
        let mut harness = Harness::new();
        let trigger = focusable_node(
            &mut harness,
            "context-rebind",
            LogicalRect::new(px(200.0), px(40.0), px(80.0), px(32.0)),
        );
        let menu = harness.menu_with(
            vec![MenuEntry::Action(MenuAction::new("copy", "复制"))],
            options(),
        );
        menu.bind_trigger(harness.app.app_mut(), harness.window, trigger)
            .expect("首次绑定");
        menu.bind_trigger(harness.app.app_mut(), harness.window, trigger)
            .expect("重复绑定");
        let _ = harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(trigger));
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::F10, Modifiers::SHIFT));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::F10, Modifiers::SHIFT));
        harness.settle(&menu);
        assert!(
            menu.is_open(harness.app.app()),
            "重复绑定后 Shift+F10 仍打开"
        );
        assert_eq!(
            harness
                .app
                .app()
                .popup_stack(harness.window)
                .expect("栈")
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn the_item_click_callback_reports_the_new_check_state() {
        let mut harness = Harness::new();
        let reported: Rc<RefCell<Vec<(SmolStr, Option<MenuCheckState>)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let sink = reported.clone();
        let menu = harness
            .menu_with(
                vec![MenuEntry::Action(
                    MenuAction::new("toggle", "开关").checkbox(false),
                )],
                options(),
            )
            .on_item_click(move |_, id, check| sink.borrow_mut().push((id, check)));
        harness.open(&menu, LogicalPoint::new(px(300.0), px(200.0)));
        harness.key(&menu, KeyCode::Enter);
        let reported = reported.borrow();
        assert_eq!(reported.len(), 1, "Enter 激活一次只回调一次");
        assert_eq!(reported[0].0, SmolStr::new_static("toggle"));
        assert_eq!(
            reported[0].1,
            Some(MenuCheckState {
                kind: MenuCheck::Checkbox,
                checked: true,
            }),
            "on_item_click 收到切换后的 check 状态"
        );
    }
}

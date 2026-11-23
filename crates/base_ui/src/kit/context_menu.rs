//! 上下文菜单（P2-4c：旧 `crate::context_menu` 的引擎化重建）。
//!
//! 旧实现是「窗口级单例菜单」：`Layer` 拥有它，全屏透明 backdrop 吃掉菜单外的按下，面板由
//! `Positioner::corner(anchor, position)` 按锚点角摆放，点击条目 / 外部 / `dismiss` 关闭，关闭时
//! 调一次 `on_close`。新引擎已具备这些原语，本模块把语义映射过来：
//!
//! | 旧语义 | 新落点 |
//! |---|---|
//! | 全屏 backdrop 吃按下 | 引擎浮层 `OverlayDismiss::OutsideClick`（整次按下被消费） |
//! | `Positioner::corner` | [`Anchor::point`] + [`Placement::BOTTOM_START`]（菜单左上角在点上）/ [`Placement::TOP_START`]（左下角在点上） |
//! | `Layer` 拥有 + 至多一个 | [`Layer::close_top_of`]：打开前关掉同层带（[`Band::ContextMenu`]）里的旧菜单 |
//! | 点击条目先关再上报 | 激活回调里 `close_now`，再调 `on_item_click` |
//! | `on_close` 每次关闭一次 | [`Menu::on_close`] 的同一事件（点击外部 / Esc / 选中都算） |
//! | 视口边距 4 px | 浮层 `margin = 4`（`resolve_origin` 的 clamp） |
//!
//! 菜单本身（条目 / 分组 / 分隔线 / 快捷键 / 子菜单 / 危险项 / 键盘导航 / a11y）由
//! [`crate::kit::menu::Menu`] 提供；本模块只负责「右键坐标 → 浮层锚点」与「窗口级单例 / 关闭」。
//!
//! 旧实现每帧重建条目（`MenuBuilder`），新实现要求调用方在 [`ContextMenu::open`] 之前把条目
//! 准备好（[`ContextMenu::set_entries`] / 构造时给）；打开后菜单内容固定。

use std::{cell::RefCell, fmt, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, math::Point, node::id::NodeId, window::WindowId};

use crate::kit::{
    layer::{Band, Layer},
    menu::{Menu, MenuEntry, MenuMode, MenuOptions},
    overlay::{Anchor, AnimationRouter, OverlayError, Placement},
    size::UnifiedSize,
};

/// 菜单的哪个角落落在右键点上（旧锚点类型）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuAnchor {
    /// 菜单左上角在点上（菜单向右下展开；旧的默认）。
    #[default]
    TopLeft,
    /// 菜单左下角在点上（菜单向右上展开；旧的行底边菜单）。
    BottomLeft,
}

impl MenuAnchor {
    fn placement(self) -> Placement {
        match self {
            Self::TopLeft => Placement::BOTTOM_START,
            Self::BottomLeft => Placement::TOP_START,
        }
    }
}

/// 上下文菜单参数。
#[derive(Clone, Debug, PartialEq)]
pub struct ContextMenuOptions {
    /// 尺寸档。
    pub size: UnifiedSize,
    /// 面板宽度（旧构造函数的 `width` 参数）。
    pub width: f32,
    /// 面板最大高度（超出滚动）。
    pub max_height: f32,
    /// 哪个角落落在右键点上。
    pub anchor: MenuAnchor,
    /// 菜单可访问名。
    pub label: Option<SmolStr>,
}

impl Default for ContextMenuOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            width: 200.0,
            max_height: 320.0,
            anchor: MenuAnchor::TopLeft,
            label: None,
        }
    }
}

impl ContextMenuOptions {
    /// 面板宽度。
    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// 面板最大高度。
    #[must_use]
    pub fn max_height(mut self, max_height: f32) -> Self {
        self.max_height = max_height;
        self
    }

    /// 锚点角。
    #[must_use]
    pub fn anchor(mut self, anchor: MenuAnchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// 菜单可访问名。
    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// 上下文菜单句柄（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct ContextMenu {
    inner: Rc<ContextMenuInner>,
}

struct ContextMenuInner {
    key: SmolStr,
    layer: Layer,
    menu: RefCell<Menu>,
    on_item_click: RefCell<Option<Rc<dyn Fn(&mut App, SmolStr)>>>,
    window: RefCell<Option<WindowId>>,
}

impl ContextMenu {
    /// 建上下文菜单（关闭态）。
    pub fn new(
        app: &mut App,
        theme: crate::theme::handle::ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        options: ContextMenuOptions,
    ) -> Self {
        let key = key.into();
        let menu = Menu::new(
            app,
            theme,
            layer.clone(),
            router,
            format_smolstr!("{key}:menu"),
            entries,
            menu_options(&options),
        );
        let inner = Rc::new(ContextMenuInner {
            key,
            layer,
            menu: RefCell::new(menu),
            on_item_click: RefCell::new(None),
            window: RefCell::new(None),
        });
        let context_menu = Self { inner };
        context_menu.install_handlers();
        context_menu
    }

    /// 菜单键（调试 id / 动作前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 是否打开。
    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner.menu.borrow().is_open(app)
    }

    /// 菜单面板节点（打开时）。
    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.menu.borrow().panel_node()
    }

    /// 高亮行。
    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.menu.borrow().highlighted(app)
    }

    /// 换一批条目（菜单关闭时也可以先设好，`open` 时生效）。
    ///
    /// # Errors
    /// 引擎虚拟列表失效。
    pub fn set_entries(
        &self,
        app: &mut App,
        entries: impl IntoIterator<Item = MenuEntry>,
    ) -> Result<(), EngineError> {
        self.inner.menu.borrow().set_entries(app, entries)
    }

    /// 条目快照。
    #[must_use]
    pub fn entries(&self) -> Rc<Vec<MenuEntry>> {
        self.inner.menu.borrow().entries()
    }

    /// 在 `point`（窗口逻辑坐标）处打开菜单。
    ///
    /// 同层带里已有的上下文菜单会被关掉（旧 `Layer` 的「至多一个」语义）。
    ///
    /// # Errors
    /// 浮层参数 / 引擎建树错误。
    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        point: Point,
    ) -> Result<bool, OverlayError> {
        self.close_peers(app, window);
        *self.inner.window.borrow_mut() = Some(window);
        self.inner
            .menu
            .borrow()
            .open(app, window, Anchor::point(point))
    }

    /// 打开菜单并把高亮停到指定条目 id（右键命中的那一项）。
    ///
    /// # Errors
    /// 同 [`ContextMenu::open`]。
    pub fn open_at(
        &self,
        app: &mut App,
        window: WindowId,
        point: Point,
        highlighted: Option<&str>,
    ) -> Result<bool, OverlayError> {
        self.close_peers(app, window);
        *self.inner.window.borrow_mut() = Some(window);
        self.inner
            .menu
            .borrow()
            .open_at(app, window, Anchor::point(point), highlighted)
    }

    /// 关闭菜单（先关子菜单）。
    pub fn close(&self, app: &mut App) -> bool {
        self.inner.menu.borrow().close(app)
    }

    /// 立即关闭菜单。
    pub fn close_now(&self, app: &mut App) -> bool {
        self.inner.menu.borrow().close_now(app)
    }

    /// 条目点击回调（菜单先关闭，再上报 id —— 旧 `on_item_click` 的语义）。
    pub fn on_item_click(self, callback: impl Fn(&mut App, SmolStr) + 'static) -> Self {
        *self.inner.on_item_click.borrow_mut() = Some(Rc::new(callback));
        self
    }

    /// 关闭回调（点击外部 / Esc / 选中条目都算；每次打开最多一次）。
    pub fn on_close(self, callback: impl Fn(&mut App) + 'static) -> Self {
        let menu = self.inner.menu.borrow().clone().on_close(callback);
        *self.inner.menu.borrow_mut() = menu;
        self
    }

    /// 帧同步（浮层 / 子菜单 / 悬停）。
    ///
    /// # Errors
    /// 引擎列表同步错误。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        self.inner.menu.borrow().sync(app)
    }

    /// 释放组件（菜单 / 子菜单 / 信号作用域）。
    pub fn release(&self, app: &mut App) -> usize {
        self.inner.menu.borrow().release(app)
    }

    // ── 内部 ──

    /// 菜单激活 ⇒ 先关菜单再上报。
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
                    callback(app, activation.id);
                }
            });
        *self.inner.menu.borrow_mut() = menu;
    }

    /// 关掉同层带里别的上下文菜单（旧 `Layer` 的单例语义）；自己的根若已是最上层则保留，
    /// 自己的子菜单（同带的嵌套层）先关掉。
    ///
    /// 归属比较用**引擎浮层包装节点**（`Layer::Entry::node`）而不是 kit 面板节点：
    /// 两者是不同的 `NodeId`，用面板节点会把自己的根也关掉、然后在 `open` 里重建（审查 F26
    /// 顺带修正）。
    fn close_peers(&self, app: &mut App, window: WindowId) {
        let own = self.inner.menu.borrow().overlay_node();
        while let Some(top) = self.inner.layer.top_of(window, Band::ContextMenu) {
            if Some(top.node()) == own {
                break;
            }
            if !self
                .inner
                .layer
                .close_top_of(app, window, Band::ContextMenu)
            {
                break;
            }
        }
    }
}

/// 上下文菜单的菜单参数（层带恒为 `ContextMenu`，角色恒为 `Menu`）。
fn menu_options(options: &ContextMenuOptions) -> MenuOptions {
    MenuOptions {
        size: options.size,
        width: options.width,
        max_height: options.max_height,
        mode: MenuMode::Menu,
        placement: options.anchor.placement(),
        gap: 0.0,
        band: Band::ContextMenu,
        label: options.label.clone(),
        submenu_delay: std::time::Duration::from_millis(150),
    }
}

impl fmt::Debug for ContextMenu {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextMenu")
            .field("key", &self.inner.key)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    //! 引擎树上的行为：锚点角 / 视口 clamp / 单例 / 关闭语义 / 条目上报顺序 / a11y。

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
        math::{Point, Rect, Size},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{ContextMenu, ContextMenuOptions, MenuAnchor, MenuEntry};
    use crate::{
        kit::{
            layer::{Band, Layer},
            menu::{MenuAction, MenuSubmenu},
            overlay::AnimationRouter,
            test_support::{a11y_snapshot_without_oracle, overlay_wrapper},
        },
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
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
            NodeBuilder::boxed()
                .id("background")
                .layout_override(Rect::new(600.0, 500.0, 80.0, 40.0))
                .on_click(Action::Custom(SmolStr::new_static("background.click")))
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("背景节点");
            app.app_mut().on_action(
                Action::Custom(SmolStr::new_static("background.click")),
                move |_| sink.set(sink.get() + 1),
            );
            Self {
                app,
                window,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
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
                self.layer.clone(),
                self.router.clone(),
                "context",
                entries,
                options,
            )
            .on_item_click(move |_, id| reported.borrow_mut().push(id))
            .on_close(move |_| closes.set(closes.get() + 1))
        }

        fn open(&mut self, menu: &ContextMenu, at: Point) -> bool {
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

        fn click(&mut self, menu: &ContextMenu, at: Point) {
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

        fn bounds(&self, node: NodeId) -> Rect {
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

        fn row_center(&self, index: usize) -> Point {
            let node = self
                .find(&format!("context:menu:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"));
            let bounds = self.bounds(node);
            Point::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        /// 把指针移到 `at`，再推进帧（悬停开子菜单的定时器在 `sync` 里触发）。
        fn hover(&mut self, menu: &ContextMenu, at: Point) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(at, Modifiers::empty()));
            self.settle(menu);
        }

        /// root 的子节点（每个浮层一个包装节点，登记序 = z 序）。
        fn root_children(&self) -> Vec<NodeId> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            tree.get(tree.root()).expect("root").children().to_vec()
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
            width: 200.0,
            ..ContextMenuOptions::default()
        }
    }

    #[test]
    fn the_menu_opens_with_its_top_left_corner_on_the_point() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        let point = Point::new(300.0, 200.0);
        assert!(harness.open(&menu, point));
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert_eq!(panel.origin, point, "TopLeft 锚点：面板左上角在点上");
        assert_eq!(panel.size.width, 200.0, "面板宽度 = 构造参数");
    }

    #[test]
    fn a_bottom_left_menu_grows_up_from_its_point() {
        let mut harness = Harness::new();
        let menu = harness.menu(ContextMenuOptions {
            anchor: MenuAnchor::BottomLeft,
            ..options()
        });
        let point = Point::new(300.0, 300.0);
        assert!(harness.open(&menu, point));
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert_eq!(panel.origin.x, point.x, "左下角锚点：左边缘在点上");
        assert!(
            (panel.max().y - point.y).abs() < 0.5,
            "左下角锚点：底边在点上（实际 {panel:?} / 点 {point:?}）"
        );
    }

    #[test]
    fn the_menu_stays_inside_the_viewport_near_an_edge() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        assert!(harness.open(&menu, Point::new(790.0, 590.0)));
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.max().x <= 800.0 - 4.0 + 0.5 && panel.max().y <= 600.0 - 4.0 + 0.5,
            "贴边时被 clamp 进视口（留 4 px 边距）：{panel:?}"
        );
    }

    #[test]
    fn choosing_an_entry_closes_the_menu_before_reporting_it() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        harness.open(&menu, Point::new(300.0, 200.0));
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
    fn a_press_outside_closes_the_menu_and_does_not_reach_the_page() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        harness.open(&menu, Point::new(300.0, 200.0));
        harness.click(&menu, Point::new(640.0, 520.0));
        assert!(!menu.is_open(harness.app.app()), "外部按下关闭菜单");
        assert_eq!(
            harness.background_clicks.get(),
            0,
            "浮层吞掉这次按下（旧 backdrop 的 BlockMouse 语义）"
        );
    }

    #[test]
    fn escape_closes_the_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        harness.open(&menu, Point::new(300.0, 200.0));
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()));
        assert_eq!(harness.closes.get(), 1);

        // 调用方主动关闭（旧 `a_menu_should_close_when_its_caller_asks`）。
        harness.open(&menu, Point::new(300.0, 200.0));
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
        harness.open(&first, Point::new(300.0, 200.0));
        assert!(first.is_open(harness.app.app()));
        harness.open(&second, Point::new(400.0, 300.0));
        harness.settle(&first);
        assert!(
            !first.is_open(harness.app.app()),
            "同层带至多一个（旧 Layer 语义）"
        );
        assert!(second.is_open(harness.app.app()));
    }

    #[test]
    fn reopening_closes_the_previous_submenu() {
        // 审查 F26：`close_peers` 必须在**自身重开**时关掉旧子菜单（同带嵌套层）。只测
        // 「第二个菜单替换第一个」不够 —— 那条由 `Layer::Exclusive` 槽位兜底，`close_peers`
        // 改 no-op 后仍然绿。
        let mut harness = Harness::new();
        let menu = harness.menu_with(entries_with_submenu(), options());
        harness.open(&menu, Point::new(300.0, 200.0));
        let root_panel = menu.panel_node().expect("根面板");
        let at = harness.row_center(1);
        harness.hover(&menu, at);
        assert!(
            harness
                .find("context:menu:sub:1:panel:list:row:0")
                .is_some(),
            "悬停打开子菜单"
        );
        assert_eq!(harness.layer.len(harness.window), 2, "根菜单 + 子菜单两层");

        harness.open(&menu, Point::new(200.0, 150.0));
        assert!(
            harness
                .find("context:menu:sub:1:panel:list:row:0")
                .is_none(),
            "自身重开时旧子菜单被关掉"
        );
        assert_eq!(
            menu.panel_node(),
            Some(root_panel),
            "自己的根被保留（只换锚点，不重建面板）"
        );
        let top = harness
            .layer
            .top_of(harness.window, Band::ContextMenu)
            .expect("根菜单仍在层带里");
        let tree = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree();
        let mut cursor = Some(root_panel);
        let mut top_is_root = false;
        while let Some(node) = cursor {
            if node == top.node() {
                top_is_root = true;
                break;
            }
            cursor = tree.get(node).ok().and_then(|node| node.parent());
        }
        assert!(top_is_root, "层带顶部是自己的根浮层（而非旧子菜单）");
        assert_eq!(harness.layer.len(harness.window), 1, "层带只剩根菜单一层");
    }

    #[test]
    fn open_at_puts_the_highlight_on_the_named_entry() {
        let mut harness = Harness::new();
        let menu = harness.menu(options());
        let opened = menu
            .open_at(
                harness.app.app_mut(),
                harness.window,
                Point::new(300.0, 200.0),
                Some("paste"),
            )
            .expect("打开");
        assert!(opened);
        harness.settle(&menu);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
    }

    /// a11y 角色 / 标签由 [`crate::kit::menu`] 的 `ItemPanel` 声明（E12 之后落在引擎浮层包装
    /// 节点上），本模块只把 `ContextMenuOptions::label` 透传过去 —— 这里断言透传结果。
    /// 全树 oracle 在本组合下会与增量分叉（引擎 a11y 的焦点脏位问题，见 spec-delta 残余），
    /// 所以这里不做全树断言。
    #[test]
    fn the_options_label_reaches_the_menu_panel() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(ContextMenuOptions {
            label: Some(SmolStr::new_static("编辑")),
            ..options()
        });
        harness.open(&menu, Point::new(300.0, 200.0));
        assert!(menu.is_open(harness.app.app()));
        let _ = harness.app.drain().expect("帧");
        harness.app.assert_a11y_contract();
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let panel = menu.panel_node().expect("面板");
        let wrapper = overlay_wrapper(harness.app.app(), harness.window, panel);
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

    /// 含子菜单的条目（审查 P1-1：context menu 的子菜单必须继承 `ContextMenu` 层带）。
    fn entries_with_submenu() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("copy", "复制")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [MenuEntry::Action(MenuAction::new("inner", "内部"))],
            )),
        ]
    }

    /// 背景页面：一个可滚动容器（内容 3 屏高）。
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
                        .width(800.0)
                        .height(600.0)
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
            .set_scroll_content(&handle, Size::new(800.0, 2400.0))
            .expect("内容");
        handle
    }

    /// 在 `at` 处滚一格并推进到平滑滚动结束。
    fn wheel(harness: &mut Harness, menu: &ContextMenu, at: Point) {
        let _ = harness.app.inject(InputEvent::scroll_lines(
            at,
            Point::new(0.0, -2.0),
            Modifiers::empty(),
        ));
        harness.settle(menu);
        for _ in 0..8 {
            harness.app.advance(Duration::from_millis(40));
            let _ = harness.app.drain().expect("帧");
        }
    }

    fn page_offset(harness: &Harness, handle: &ui_core::interaction::ScrollHandle) -> f32 {
        harness
            .app
            .app()
            .scroll_state(handle)
            .expect("滚动状态")
            .offset
            .y
    }

    #[test]
    fn a_submenu_keeps_the_context_menu_band_and_escape_closes_it_first() {
        let mut harness = Harness::new();
        let menu = harness.menu_with(entries_with_submenu(), options());
        harness.open(&menu, Point::new(300.0, 200.0));
        let root_panel = menu.panel_node().expect("根面板");
        let root_wrapper = overlay_wrapper(harness.app.app(), harness.window, root_panel);
        let before = harness.root_children();
        assert_eq!(
            before.iter().filter(|node| **node == root_wrapper).count(),
            1,
            "根浮层是 root 的直接子节点"
        );

        let at = harness.row_center(1);
        harness.hover(&menu, at);
        let children = harness.root_children();
        let submenu_row = harness
            .find("context:menu:sub:1:panel:list:row:0")
            .unwrap_or_else(|| {
                let tree = harness
                    .app
                    .app()
                    .window(harness.window)
                    .expect("窗口")
                    .tree();
                let ids: Vec<String> = tree
                    .get(tree.root())
                    .expect("root")
                    .children()
                    .iter()
                    .map(|node| format!("{:?}", tree.debug_id(*node)))
                    .collect();
                panic!("子菜单行未挂载；root children = {ids:?}");
            });
        let submenu_wrapper = overlay_wrapper(harness.app.app(), harness.window, submenu_row);
        let root_index = children.iter().position(|node| *node == root_wrapper);
        let submenu_index = children.iter().position(|node| *node == submenu_wrapper);
        assert!(
            submenu_index > root_index,
            "子菜单在父面板之后排序（z 序在上）：root {root_index:?} / submenu {submenu_index:?}"
        );

        // `Layer` 顶部仍在 `ContextMenu` 带（子菜单继承父带，而不是降到 `Popup`）。
        let top = harness
            .layer
            .top_of(harness.window, Band::ContextMenu)
            .expect("层带里有菜单");
        let tree = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree();
        let mut cursor = Some(submenu_row);
        let mut top_is_submenu = false;
        while let Some(node) = cursor {
            if node == top.node() {
                top_is_submenu = true;
                break;
            }
            cursor = tree.get(node).ok().and_then(|node| node.parent());
        }
        assert!(top_is_submenu, "层带顶部是子菜单面板（而非父菜单）");

        // 第一次 Esc 只关子菜单，父菜单仍在；第二次 Esc 才关父。
        harness.key(&menu, KeyCode::Escape);
        assert!(menu.is_open(harness.app.app()), "Esc 先关子菜单");
        assert!(
            harness
                .find("context:menu:sub:1:panel:list:row:0")
                .is_none(),
            "子菜单已关闭"
        );
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()), "第二次 Esc 关父菜单");
        assert_eq!(harness.closes.get(), 1, "根菜单只上报一次关闭");
    }

    #[test]
    fn the_page_does_not_scroll_under_an_open_context_menu() {
        let mut harness = Harness::new();
        let page = scroll_page(&mut harness);
        let menu = harness.menu(options());
        harness.open(&menu, Point::new(300.0, 200.0));

        // 面板外滚轮：模态作用域消费，页面不滚（旧全屏 backdrop 的语义）。
        wheel(&mut harness, &menu, Point::new(700.0, 500.0));
        assert_eq!(page_offset(&harness, &page), 0.0, "面板外滚轮不滚页面");
        // 面板上滚轮：滚动链只到浮层内，页面同样不滚。
        let on_panel = harness.row_center(0);
        wheel(&mut harness, &menu, on_panel);
        assert_eq!(page_offset(&harness, &page), 0.0, "面板上滚轮不滚页面");

        // 对照：关掉菜单后同一位置的滚轮真的能滚页面（证明容器有效）。
        harness.key(&menu, KeyCode::Escape);
        wheel(&mut harness, &menu, Point::new(700.0, 500.0));
        assert!(
            page_offset(&harness, &page) > 50.0,
            "无浮层时页面可滚：{}",
            page_offset(&harness, &page)
        );
    }
}

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
use ui_core::{
    app::App, error::EngineError, event::Event, math::Point, node::id::NodeId, window::WindowId,
};

use crate::kit::{
    layer::{Band, Layer},
    menu::{Menu, MenuEntry, MenuMode, MenuOptions},
    overlay::{Anchor, OverlayError, Placement},
    size::UnifiedSize,
};

/// 菜单的哪个角落落在右键点上（旧 `gpui_kit::Anchor`）。
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
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        options: ContextMenuOptions,
    ) -> Self {
        let key = key.into();
        let menu = Menu::new(
            app,
            theme,
            layer.clone(),
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

    /// 转发平台事件（菜单行点击兜底 + 子菜单）。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.inner.menu.borrow().dispatch(app, event)
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

    /// 关掉同层带里别的上下文菜单（旧 `Layer` 的单例语义）。
    fn close_peers(&self, app: &mut App, window: WindowId) {
        let own = self.inner.menu.borrow().panel_node();
        if let Some(top) = self.inner.layer.top_of(window, Band::ContextMenu) {
            let is_own = own.is_some_and(|own| {
                self.inner
                    .menu
                    .borrow()
                    .panel_node()
                    .is_some_and(|panel| panel == own)
                    && top.node() == own
            });
            if !is_own {
                let _ = self
                    .inner
                    .layer
                    .close_top_of(app, window, Band::ContextMenu);
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
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{ContextMenu, ContextMenuOptions, MenuAnchor, MenuEntry};
    use crate::{
        kit::{layer::Layer, menu::MenuAction},
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        layer: Layer,
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
                reported: Rc::new(RefCell::new(Vec::new())),
                closes: Rc::new(Cell::new(0)),
                background_clicks: clicks,
            }
        }

        fn menu(&mut self, options: ContextMenuOptions) -> ContextMenu {
            let reported = self.reported.clone();
            let closes = self.closes.clone();
            ContextMenu::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                "context",
                entries(),
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

    /// a11y 角色 / 标签由 [`crate::kit::menu`] 的面板声明（`Menu` + 标签），本模块只把
    /// `ContextMenuOptions::label` 透传过去 —— 这里断言透传结果，角色断言见 `kit::menu` 的测试
    /// （同一 `ItemPanel` 路径）。全树 oracle 在本组合下会与增量分叉（引擎 a11y 的焦点脏位问题，
    /// 见 spec-delta 残余），所以这里不做全树断言。
    #[test]
    fn the_options_label_reaches_the_menu_panel() {
        let mut harness = Harness::new();
        let menu = harness.menu(ContextMenuOptions {
            label: Some(SmolStr::new_static("编辑")),
            ..options()
        });
        harness.open(&menu, Point::new(300.0, 200.0));
        assert!(menu.is_open(harness.app.app()));
        let _ = harness.app.drain().expect("帧");
        harness.app.assert_a11y_contract();
    }
}

//! 图层栈：固定的 z 层带 + 引擎浮层栈的排序仲裁（旧 `crate::layer` 的引擎化移植）。
//!
//! # 旧实现的语义（`crates/base_ui/src/layer.rs`，1,694 行）
//!
//! 旧 `Layer` 是**每个窗口唯一的根 view 实体**，不是多窗口 / 多 Pane 栈：`view` 在最底下，
//! 之上依次是 `dialogs`（旧→新）、`alert`（至多一个，恒高于所有 dialog）、`context_menu`
//! （恒高于 alert），普通弹层（dropdown / select / tooltip 等）由 GPUI 的 `deferred(priority)`
//! 画在开它的层之上。它自己的职责只有三件：**层序**、**遮罩**（dialog / alert 的
//! `BlockMouse`）、**每层的内容所有权**（dialog / alert / menu 的实体栈）。
//!
//! # 引擎化后的塌缩
//!
//! 新引擎把「一个浮层」做成 root 的**末尾子节点**（[`ui_core::app::App::create_overlay`]）：
//! 绘制序 = DFS 先序末尾，命中优先级 = 逆序命中 —— 于是**创建序就是 z 序**，没有 GPUI 的
//! deferred / priority / BlockMouse。旧实现里「alert 恒高于所有 dialog **含后开的**」这条
//! 固定层序（评估 G6）无法用创建序表达，本模块就是补丁：用 [`Band`] 给每个浮层标一个固定
//! 层带，[`Layer::enforce`] 按「(层带, 登记序)」把 root 子节点顺序对齐到层带表。
//!
//! 因此本模块**不拥有内容**（dialog / alert / menu 的实体栈属于各自的迁移批次），只拥有：
//!
//! - **层带表**：`View < Dialog < Alert < Popup < ContextMenu < Tooltip`（旧 layer + POPUP_PRIORITY
//!   的等价次序：普通弹层在 alert 之上，context menu 在弹层之上，tooltip 最高）；
//! - **排序仲裁**：登记 / 移除 / 提前 / 关闭最上层，以及把引擎的 root 子节点顺序对齐到层带；
//! - **层内焦点与点击穿透的查询面**：最上层模态（焦点作用域根）、最上层是哪个浮层（命中优先）。
//!
//! # 与 overlay 的分工
//!
//! [`crate::kit::overlay::Overlay`] 负责「一个浮层」的定位 / 遮罩 / 淡入淡出 / 关闭策略；
//! `Layer` 负责「多个浮层之间」谁压谁。前者 `open` 时把引擎句柄登记到这里，`close` 时移除；
//! 帧同步时 `Overlay::sync` 调 [`Layer::enforce`] 让层带次序生效（只有失配时才重排，I3）。
//!
//! # 遮罩与点击穿透
//!
//! 遮罩不是本模块画的节点：每个模态浮层的遮罩是**浮层自己的子节点**（与浮层同生命周期，
//! 见 `overlay` 模块），它覆盖整个窗口且 `interactive: false`（命中穿透到背景）。于是：
//!
//! - 非模态层不画遮罩，浮层包围盒之外的下层控件照常可点（旧实现的普通 popup 同义）；
//! - 模态层由引擎 `trap_focus` 消费浮层外的整次按下（不抢焦点、不触发下层动作），遮罩只负责
//!   视觉与「点遮罩 = 点外部」的命中来源（遮罩不可命中 ⇒ 命中的是背景节点 ⇒ 引擎的
//!   `outside_click_target` 判定「点在浮层外」）。
//!
//! # 嵌套层
//!
//! 浮层嵌套由引擎的 `opener` 链表达（模态里的下拉 / 菜单仍属于该模态的焦点作用域，
//! `interaction::overlay::in_modal_scope`）。层带保证子菜单（ContextMenu）画在菜单（Popup）
//! 之上、提示（Tooltip）画在菜单之上；opener 链保证它们的 Tab 序仍在父模态之内。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use ui_core::{
    app::App, interaction::Overlay as EngineOverlay, node::id::NodeId, window::WindowId,
};

/// 固定层带（旧 `layer.rs` 的四段层序 + `POPUP_PRIORITY` 的等价次序）。
///
/// 数值只用于排序，**不是**像素；同一层带内按登记序（后登记 = 更上）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    /// 窗口根视图（旧 `Layer::view`）：不是浮层，任何浮层都在它之上，仅用于表达「底」。
    View,
    /// 对话框栈（旧 `dialogs`，旧→新）：后开的 dialog 在旧的之上。
    Dialog,
    /// 提示框（旧 `alert`）：恒高于所有 dialog，**包括后开的 dialog**。
    Alert,
    /// 普通弹层（旧 `POPUP_PRIORITY`）：dropdown / select / menu / color_picker 的面板。
    Popup,
    /// 上下文菜单（旧 `CONTEXT_MENU_PRIORITY = POPUP_PRIORITY + 1`）。
    ContextMenu,
    /// tooltip（旧 `POPUP_PRIORITY + 2`）：菜单项上的提示不被菜单埋住。
    Tooltip,
}

impl Band {
    /// 全部层带，从底到顶。
    pub const ALL: [Self; 6] = [
        Self::View,
        Self::Dialog,
        Self::Alert,
        Self::Popup,
        Self::ContextMenu,
        Self::Tooltip,
    ];

    /// 排序键（层带序号，小的在下面）。
    #[must_use]
    pub const fn order(self) -> u8 {
        match self {
            Self::View => 0,
            Self::Dialog => 10,
            Self::Alert => 20,
            Self::Popup => 30,
            Self::ContextMenu => 40,
            Self::Tooltip => 50,
        }
    }
}

/// 一个已登记的层：引擎浮层句柄 + 层带 + 模态标记。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 层带。
    pub band: Band,
    /// 引擎浮层句柄（`App::create_overlay` 铸出）。
    pub overlay: EngineOverlay,
    /// 是否模态（焦点作用域根 + 遮罩由浮层自己画）。
    pub modal: bool,
    /// 登记序号（单调；同层带内后登记的在上面）。
    pub sequence: u64,
}

impl Entry {
    /// 该层的浮层节点。
    #[must_use]
    pub fn node(self) -> NodeId {
        self.overlay.node()
    }
}

struct LayerInner {
    entries: RefCell<Vec<Entry>>,
    next_sequence: Cell<u64>,
}

/// 图层栈句柄（`Rc` 浅拷贝：浮层与组合根共享同一份栈）。
#[derive(Clone)]
pub struct Layer {
    inner: Rc<LayerInner>,
}

impl Default for Layer {
    fn default() -> Self {
        Self::new()
    }
}

impl Layer {
    /// 空栈。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Rc::new(LayerInner {
                entries: RefCell::new(Vec::new()),
                next_sequence: Cell::new(0),
            }),
        }
    }

    /// 登记一层（返回登记序号）。
    ///
    /// 登记只改变本栈的次序记录，不动引擎树；下一次 [`Layer::enforce`] 才把 root 子节点
    /// 顺序对齐（浮层通常是刚由 `App::create_overlay` 挂到 root 末尾的，所以这一帧往往已经
    /// 在正确位置，`enforce` 会零成本早退）。
    pub fn register(&self, band: Band, overlay: EngineOverlay, modal: bool) -> u64 {
        let sequence = self.inner.next_sequence.get();
        self.inner.next_sequence.set(sequence + 1);
        self.inner.entries.borrow_mut().push(Entry {
            band,
            overlay,
            modal,
            sequence,
        });
        sequence
    }

    /// 移除一层（不存在 ⇒ `false`）。
    pub fn remove(&self, overlay: &EngineOverlay) -> bool {
        let mut entries = self.inner.entries.borrow_mut();
        let before = entries.len();
        entries.retain(|entry| entry.overlay != *overlay);
        entries.len() != before
    }

    /// 已登记层数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.entries.borrow().len()
    }

    /// 是否空栈。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 全部层，**自下而上**（层带升序，同带按登记序）。
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        let mut entries = self.inner.entries.borrow().clone();
        entries.sort_unstable_by_key(|entry| (entry.band, entry.sequence));
        entries
    }

    /// 最上层（自下而上最后一项）。
    #[must_use]
    pub fn top(&self) -> Option<Entry> {
        self.entries().pop()
    }

    /// 某层带的最上层。
    #[must_use]
    pub fn top_of(&self, band: Band) -> Option<Entry> {
        self.entries()
            .into_iter()
            .rev()
            .find(|entry| entry.band == band)
    }

    /// 最上层**模态**层（焦点作用域根；`None` = 窗口没有模态浮层）。
    ///
    /// 与引擎 `interaction::overlay::focus_scope_root` 同源：最上层带 `trap_focus` 的浮层说了算，
    /// 非模态层不改变焦点作用域。
    #[must_use]
    pub fn modal_scope(&self) -> Option<Entry> {
        self.entries().into_iter().rev().find(|entry| entry.modal)
    }

    /// 该节点是否是某个已登记层的浮层节点。
    #[must_use]
    pub fn contains_node(&self, node: NodeId) -> bool {
        self.inner
            .entries
            .borrow()
            .iter()
            .any(|entry| entry.node() == node)
    }

    /// 某浮层在当前栈里的高度序号（自下而上，`0` = 最底；未登记 ⇒ `None`）。
    #[must_use]
    pub fn position(&self, overlay: &EngineOverlay) -> Option<usize> {
        self.entries()
            .iter()
            .position(|entry| entry.overlay == *overlay)
    }

    /// 把引擎 root 子节点顺序对齐到层带表；返回是否真的重排了（已对齐 ⇒ `false`，I3）。
    ///
    /// 只要求**已登记浮层之间的相对顺序**正确：窗口里若有本栈未登记的浮层（理论上不应存在，
    /// 组合根应把所有浮层登记到同一个栈），它们的相对位置不参与判定；一旦需要重排，已登记层
    /// 会被整体提到它们之上 —— 登记的层恒在未登记的 root 子节点之上，语义确定。
    ///
    /// 重排用 [`ui_core::app::App::bring_overlay_to_front`]（把节点重挂到 root 末尾），按层带
    /// 自下而上依次调用，最终顺序即层带顺序。整条路径只在失配时发生，稳态零成本。
    pub fn enforce(&self, app: &mut App, window: WindowId) -> bool {
        let desired: Vec<NodeId> = self.entries().iter().map(|entry| entry.node()).collect();
        if desired.is_empty() {
            return false;
        }
        let current: Vec<(NodeId, _)> = app.overlays(window);
        let current_registered: Vec<NodeId> = current
            .iter()
            .map(|(node, _)| *node)
            .filter(|node| self.contains_node(*node))
            .collect();
        if current_registered == desired {
            return false;
        }
        // 未登记浮层不参与排序：把它们留在原位，只把登记层按次序提到末尾。
        for entry in self.entries() {
            app.bring_overlay_to_front(&entry.overlay);
        }
        true
    }

    /// 关闭最上层（自下而上最后一项）；引擎负责恢复焦点。返回是否有层被关闭。
    pub fn close_top(&self, app: &mut App) -> bool {
        let Some(entry) = self.top() else {
            return false;
        };
        let closed = app.close_overlay(&entry.overlay);
        self.remove(&entry.overlay);
        closed
    }

    /// 关闭某层带的最上层（旧「第二次 alert 替换第一次」/「新 context menu 替换旧的」的
    /// 通用入口）。返回是否有层被关闭。
    pub fn close_top_of(&self, app: &mut App, band: Band) -> bool {
        let Some(entry) = self.top_of(band) else {
            return false;
        };
        let closed = app.close_overlay(&entry.overlay);
        self.remove(&entry.overlay);
        closed
    }
}

#[cfg(test)]
mod tests {
    //! 层带的**纯逻辑**断言：不需要窗口。需要引擎树的地方在 `ui_tests`。

    use super::Band;

    #[test]
    fn bands_order_from_view_to_tooltip() {
        assert_eq!(Band::ALL.len(), 6);
        for window in Band::ALL.windows(2) {
            assert!(
                window[0].order() < window[1].order(),
                "{:?} 必须在 {:?} 之下",
                window[0],
                window[1]
            );
        }
        // 旧实现的固定关系（评估 G6）：alert 恒高于 dialog，context menu 高于 popup，tooltip 最高。
        assert!(Band::Alert.order() > Band::Dialog.order());
        assert!(Band::ContextMenu.order() > Band::Popup.order());
        assert!(Band::Tooltip.order() > Band::ContextMenu.order());
        assert!(Band::Popup.order() > Band::Alert.order());
        assert_eq!(Band::View.order(), 0);
    }
}

#[cfg(test)]
mod ui_tests {
    //! 需要真实引擎树的断言：层带排序落到 root 子节点顺序、命中优先、模态作用域、
    //! 点击穿透与"关闭上层露出下层"。

    use ui_core::{
        event::{Action, InputEvent, Modifiers, PointerButton},
        interaction::{OverlayDismiss, OverlayOptions, OverlayPlacement},
        math::{Point, Size},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{Band, Layer};
    use crate::style_builder::{PaintBuilder, StyleBuilder};

    const PANEL: Size = Size::new(120.0, 80.0);

    fn harness() -> (TestApp, WindowId, NodeId) {
        let app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        (app, window, root)
    }

    /// 造一个引擎浮层并挂一个固定尺寸的实心面板（root 子节点，绝对定位在 `origin`）。
    fn overlay_at(
        app: &mut TestApp,
        window: WindowId,
        origin: Point,
        dismiss: OverlayDismiss,
        trap_focus: bool,
    ) -> ui_core::interaction::Overlay {
        let overlay = app
            .app_mut()
            .create_overlay(
                window,
                OverlayOptions {
                    placement: OverlayPlacement::Point(origin),
                    dismiss,
                    trap_focus,
                    restore_focus: false,
                },
            )
            .expect("建浮层");
        let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
        NodeBuilder::boxed()
            .id("layer-panel")
            .style(
                StyleBuilder::new()
                    .width(PANEL.width)
                    .height(PANEL.height)
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .background(ui_core::paint::color::Color::WHITE)
                    .build(),
            )
            .build(tree, overlay.node())
            .expect("面板");
        overlay
    }

    fn ordered_nodes(app: &TestApp, window: WindowId) -> Vec<NodeId> {
        app.app()
            .overlays(window)
            .into_iter()
            .map(|(node, _)| node)
            .collect()
    }

    /// 建一个带动作的按钮，返回 `(node, 点击计数信号)`。
    fn button(
        app: &mut TestApp,
        window: WindowId,
        parent: NodeId,
        key: &str,
    ) -> (NodeId, ui_core::node::signal::Signal<u32>) {
        let clicks = app.app_mut().signal(0u32);
        let action = Action::Custom(smol_str::format_smolstr!("{key}.click"));
        let node = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .id(key.to_owned())
                .style(StyleBuilder::new().width(40.0).height(20.0).build())
                .paint(
                    PaintBuilder::new()
                        .background(ui_core::paint::color::Color::BLACK)
                        .build(),
                )
                .on_click(action.clone())
                .build(tree, parent)
                .expect("按钮")
        };
        app.app_mut().on_action(action, move |ctx| {
            ctx.update(&clicks, |count| *count += 1);
        });
        (node, clicks)
    }

    fn center(app: &TestApp, node: NodeId) -> Point {
        let bounds = app.probe().bounds(node).expect("节点必须有布局盒");
        Point::new(
            bounds.origin.x + bounds.size.width * 0.5,
            bounds.origin.y + bounds.size.height * 0.5,
        )
    }

    fn click(app: &mut TestApp, at: Point) {
        let _ = app.inject(InputEvent::pointer_down(
            at,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            at,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    #[test]
    fn a_dialog_opened_after_an_alert_stays_below_it() {
        // 旧 layer 的固定层序（评估 G6）：alert 恒高于所有 dialog，**包括后开的**。
        // 引擎的创建序会把后开的 dialog 画在 alert 之上，层带必须把它纠正回来。
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let alert = overlay_at(
            &mut app,
            window,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        let dialog = overlay_at(
            &mut app,
            window,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(Band::Alert, alert, true);
        layer.register(Band::Dialog, dialog, true);
        let _ = app.drain().expect("首帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![alert.node(), dialog.node()],
            "创建序：dialog 在 alert 之上"
        );

        assert!(layer.enforce(app.app_mut(), window), "层带失配必须重排");
        let _ = app.drain().expect("布局");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![dialog.node(), alert.node()],
            "层带：alert 必须在 dialog 之上"
        );
        // 命中优先：重叠位置命中最上层的 alert 的子树（命中目标是最深的子节点 —— 面板）。
        let tree = app.app().window(window).expect("窗口").tree();
        let hit = tree
            .hit_test(Point::new(40.0, 40.0))
            .map(|hit| hit.node)
            .expect("重叠处必有命中");
        let mut cursor = Some(hit);
        let mut in_alert = false;
        while let Some(current) = cursor {
            if current == alert.node() {
                in_alert = true;
                break;
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        assert!(
            in_alert,
            "重叠处命中 alert（最上层）的子树，实际命中 {hit:?}"
        );
    }

    #[test]
    fn the_order_within_a_band_is_the_registration_order() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let first = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        let second = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(Band::Dialog, first, false);
        layer.register(Band::Dialog, second, false);
        let _ = app.drain().expect("首帧");

        // 先制造一个错误次序：把先登记的提到最上。
        app.app_mut().bring_overlay_to_front(&first);
        let _ = app.drain().expect("帧");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![second.node(), first.node()]
        );

        assert!(layer.enforce(app.app_mut(), window));
        let _ = app.drain().expect("帧");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![first.node(), second.node()],
            "同层带按登记序（后登记 = 更上）"
        );
    }

    #[test]
    fn enforce_is_a_no_op_when_the_stack_already_matches_and_requests_no_frame() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let lower = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        let upper = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(Band::Dialog, lower, false);
        layer.register(Band::Popup, upper, false);
        let _ = app.drain().expect("首帧");

        assert!(!layer.enforce(app.app_mut(), window), "已对齐必须零工作");
        assert!(!layer.enforce(app.app_mut(), window), "重复调用幂等");
        // I3：对齐检查本身不产生帧。
        let outcome = app
            .app_mut()
            .frame(window)
            .expect("窗口")
            .expect("帧不失败");
        assert_eq!(
            outcome,
            ui_core::window::FrameOutcome::Idle,
            "静默检查不得请求帧"
        );
    }

    #[test]
    fn nested_layers_keep_their_bands() {
        // 对话框里开的下拉（Popup）恒在对话框之上、且仍属于对话框的模态作用域；
        // 下拉里的子菜单（ContextMenu）又在弹出层之上；提示（Tooltip）最高。
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        // 故意用「子层先开、父层后开」的注册序制造失配。
        let popup = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::OutsideClick,
            false,
        );
        let dialog = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            true,
        );
        let submenu = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::OutsideClick,
            false,
        );
        let tooltip = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(Band::Popup, popup, false);
        layer.register(Band::Dialog, dialog, true);
        layer.register(Band::ContextMenu, submenu, false);
        layer.register(Band::Tooltip, tooltip, false);
        let _ = app.drain().expect("首帧");
        assert!(layer.enforce(app.app_mut(), window));
        let _ = app.drain().expect("帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![dialog.node(), popup.node(), submenu.node(), tooltip.node()],
            "自下而上必须是 Dialog < Popup < ContextMenu < Tooltip"
        );
        assert_eq!(
            layer.modal_scope().map(|entry| entry.node()),
            Some(dialog.node()),
            "焦点作用域根是最上层模态（对话框），弹出层不改变它"
        );
    }

    #[test]
    fn removing_an_entry_keeps_the_remaining_order() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let lower = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        let upper = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(Band::Dialog, lower, false);
        layer.register(Band::Dialog, upper, false);
        assert!(layer.remove(&lower));
        assert!(!layer.remove(&lower), "重复移除返回 false");
        assert_eq!(layer.len(), 1);
        assert_eq!(layer.top().map(|entry| entry.node()), Some(upper.node()));
    }

    #[test]
    fn closing_the_top_layer_promotes_the_one_below() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let lower = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let upper = overlay_at(
            &mut app,
            window,
            Point::new(5.0, 5.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(Band::Dialog, lower, true);
        layer.register(Band::Dialog, upper, true);
        let _ = app.drain().expect("首帧");

        assert_eq!(
            layer.modal_scope().map(|entry| entry.node()),
            Some(upper.node())
        );
        assert!(layer.close_top(app.app_mut()));
        let _ = app.drain().expect("帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![lower.node()],
            "关闭上层后只剩下层"
        );
        assert_eq!(
            layer.modal_scope().map(|entry| entry.node()),
            Some(lower.node()),
            "焦点作用域回落到下一个模态"
        );
    }

    #[test]
    fn closing_a_band_leaves_the_other_bands_alone() {
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let alert = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let dialog = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(Band::Alert, alert, true);
        layer.register(Band::Dialog, dialog, true);
        layer.register(Band::ContextMenu, menu, false);
        let _ = app.drain().expect("首帧");

        assert!(layer.close_top_of(app.app_mut(), Band::Alert), "关掉 alert");
        layer.remove(&alert);
        let _ = app.drain().expect("帧");
        assert_eq!(layer.len(), 2);
        assert_eq!(layer.top_of(Band::Alert), None);
        assert!(layer.top_of(Band::Dialog).is_some());
        assert!(layer.top_of(Band::ContextMenu).is_some());
        // 定向关闭不动其它层的引擎节点。
        assert!(ordered_nodes(&app, window).contains(&dialog.node()));
        assert!(ordered_nodes(&app, window).contains(&menu.node()));
    }

    #[test]
    fn a_non_modal_layer_does_not_block_the_pointer() {
        // 点击穿透：非模态层（无遮罩 + 无 trap_focus）在自身包围盒之外不消费按下，
        // 背景按钮照常收到 click。
        let (mut app, window, root) = harness();
        let layer = Layer::new();
        let (button_node, clicks) = button(&mut app, window, root, "background");
        let popup = overlay_at(
            &mut app,
            window,
            Point::new(300.0, 300.0),
            OverlayDismiss::Explicit,
            false,
        );
        layer.register(Band::Popup, popup, false);
        let _ = app.drain().expect("首帧");

        let at = center(&app, button_node);
        click(&mut app, at);
        let _ = app.drain().expect("帧");
        assert_eq!(*app.app().read(&clicks), 1, "非模态层之外的下层控件可点");
        assert_eq!(
            ordered_nodes(&app, window),
            vec![popup.node()],
            "浮层未被误关"
        );
    }

    #[test]
    fn a_modal_layer_swallows_presses_outside_its_panel() {
        // 模态层：作用域外的按下被引擎整次消费（不抢焦点、不触发下层动作）。
        let (mut app, window, root) = harness();
        let layer = Layer::new();
        let (button_node, clicks) = button(&mut app, window, root, "background");
        let modal = overlay_at(
            &mut app,
            window,
            Point::new(300.0, 300.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(Band::Dialog, modal, true);
        let _ = app.drain().expect("首帧");

        let at = center(&app, button_node);
        click(&mut app, at);
        let _ = app.drain().expect("帧");
        assert_eq!(
            *app.app().read(&clicks),
            0,
            "模态作用域外的按下不得触发下层动作"
        );
        assert!(layer.contains_node(modal.node()));
    }
}

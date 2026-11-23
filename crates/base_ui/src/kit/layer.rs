//! 图层栈：固定的 z 层带 + 引擎浮层栈的排序仲裁（旧 `crate::layer` 的引擎化移植）。
//!
//! # 旧实现的语义（`crates/base_ui/src/layer.rs`，1,694 行）
//!
//! 旧 `Layer` 是**每个窗口唯一的根 view 实体**，不是多窗口 / 多 Pane 栈：`view` 在最底下，
//! 之上依次是 `dialogs`（旧→新）、`alert`（至多一个，恒高于所有 dialog）、`context_menu`
//! （恒高于 alert），普通弹层（dropdown / select / tooltip 等）由 旧 UI 层的 `deferred(priority)`
//! 画在开它的层之上。它自己的职责只有三件：**层序**、**遮罩**（dialog / alert 的
//! `BlockMouse`）、**每层的内容所有权**（dialog / alert / menu 的实体栈）。
//!
//! # 引擎化后的塌缩
//!
//! 新引擎把「一个浮层」做成 root 的**末尾子节点**（[`ui_core::app::App::create_overlay`]）：
//! 绘制序 = DFS 先序末尾，命中优先级 = 逆序命中 —— 于是**创建序就是 z 序**，没有 旧 UI 层的
//! deferred / priority / BlockMouse。旧实现里「alert 恒高于所有 dialog **含后开的**」这条
//! 固定层序（评估 G6）无法用创建序表达，本模块就是补丁：用 [`Band`] 给每个浮层标一个固定
//! 层带，[`Layer::enforce`] 按「(层带, 登记序)」把 root 子节点顺序对齐到层带表。
//!
//! 因此本模块**不拥有内容**（dialog / alert / menu 的实体栈属于各自的迁移批次），只拥有：
//!
//! - **层带表**：`Dialog < Alert < Popup < ContextMenu < Tooltip`（旧 layer + POPUP_PRIORITY
//!   的等价次序：普通弹层在 alert 之上，context menu 在弹层之上，tooltip 最高）；
//! - **排序仲裁**：登记 / 移除 / 提前 / 关闭最上层，以及把引擎的 root 子节点顺序对齐到层带；
//! - **层内焦点与点击穿透的查询面**：最上层模态（焦点作用域根）、最上层是哪个浮层（命中优先）；
//! - **旧 `layer.rs` 的三条槽位不变量**：打开 dialog / alert 前先收起 context menu；alert 与
//!   context menu 各自**至多一个**（后开替换先开，见 [`Layer::register`]）。这三条不依赖引擎
//!   批，属于旧 `Layer` 的可移植语义，因此留在本模块；dialog / alert / context_menu 批次的
//!   **内容所有权与实体栈**仍属于各自批次。
//!
//! # 一个 Layer / 按窗口分区
//!
//! [`Layer`] 是**应用级（组合根唯一）**的登记表，内部**按 `WindowId` 分区**：每条登记都从
//! 引擎浮层句柄取窗口（[`EngineOverlay::window`]），所有查询 / [`Layer::enforce`] / 关闭操作
//! 都只作用于指定窗口。这样多窗口共用一个 `Layer` 时，窗口 A 的 `enforce` 不会把窗口 B 的
//! 节点算进 `desired`（否则两边互相重排、永不收敛）。**不要**为每个窗口各建一个 `Layer`：
//! 组合根只有一个（与旧实现「每窗唯一 Layer」等价的是「每窗口一个分区」）。
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
//! `interaction::overlay::in_modal_scope`）。子菜单**继承父菜单的层带**并以 [`LayerSlot::Nested`]
//! 登记（同带叠放，后登记在上）—— 因此 `Popup` 菜单的子菜单画在菜单之上、`ContextMenu` 的
//! 子菜单画在父面板之上；提示（`Tooltip`）永远最高。opener 链保证它们的 Tab 序仍在父模态之内。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use ui_core::{
    app::App, interaction::Overlay as EngineOverlay, node::id::NodeId, window::WindowId,
};

/// 固定层带（旧 `layer.rs` 的四段层序 + `POPUP_PRIORITY` 的等价次序）。
///
/// 数值只用于排序，**不是**像素；同一层带内按登记序（后登记 = 更上）。窗口根视图（旧
/// `Layer::view`）不是浮层，不在表里 —— 引擎 root 永远在最底层，不需要登记。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    /// 对话框栈（旧 `dialogs`，旧→新）：后开的 dialog 在旧的之上。
    Dialog,
    /// 提示框（旧 `alert`）：恒高于所有 dialog，**包括后开的 dialog**；至多一个。
    Alert,
    /// 普通弹层（旧 `POPUP_PRIORITY`）：dropdown / select / menu / color_picker 的面板。
    Popup,
    /// 上下文菜单（旧 `CONTEXT_MENU_PRIORITY = POPUP_PRIORITY + 1`）：至多一个。
    ContextMenu,
    /// tooltip（旧 `POPUP_PRIORITY + 2`）：菜单项上的提示不被菜单埋住。
    Tooltip,
}

impl Band {
    /// 排序键（层带序号，小的在下面）。
    #[must_use]
    pub const fn order(self) -> u8 {
        match self {
            Self::Dialog => 10,
            Self::Alert => 20,
            Self::Popup => 30,
            Self::ContextMenu => 40,
            Self::Tooltip => 50,
        }
    }
}

/// 层带槽位规则（旧 `layer.rs` 的「至多一个」槽位是否适用）。
///
/// 子菜单**继承父菜单的层带**（审查 P1-1：写死 `Popup` 会让 `ContextMenu` 父面板反盖子菜单），
/// 但它不是「另一个上下文菜单」：同一菜单链的根 + 子菜单可以同带叠放。`Nested` 因此跳过
/// `Alert` / `ContextMenu` 的独占槽位替换，只按登记序排队（后登记 = 更上）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayerSlot {
    /// 独占槽位（`ContextMenu` / `Alert` 至多一个；后登记替换先登记）。
    #[default]
    Exclusive,
    /// 嵌套层（子菜单）：同带可叠放，不做槽位替换。
    Nested,
}

/// 一个已登记的层：引擎浮层句柄 + 层带 + 模态标记。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 层带。
    pub band: Band,
    /// 引擎浮层句柄（`App::create_overlay` 铸出；窗口从它取）。
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

    /// 该层所属窗口。
    #[must_use]
    pub fn window(self) -> WindowId {
        self.overlay.window()
    }
}

struct LayerInner {
    /// 全部窗口的登记（查询 / 排序按窗口过滤后使用，见模块文档「一个 Layer / 按窗口分区」）。
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

    /// 登记一层（返回登记序号），并执行旧 `layer.rs` 的槽位不变量：
    ///
    /// - 打开 `Dialog` / `Alert` 之前先收起本窗口的 `ContextMenu`（旧实现里 dialog / alert 是
    ///   模态的，菜单不能压在它们上面继续吃按下）；
    /// - `Alert` 与 `ContextMenu` 各自**至多一个**：后登记的替换先登记的（旧实现的单例槽位）。
    ///
    /// 替换靠引擎 [`App::close_overlay`] 立即卸载旧节点（旧实现在菜单替换时走 `dismiss` 退出
    /// 转场；引擎没有应用层可拦截的退出钩子，差异已登记）。被替换的浮层的 kit 状态由它自己
    /// 的下一次 `Overlay::sync` 收尾（关闭路径统一走 `finish_close`，登记不会泄漏）。
    ///
    /// 登记只改变本栈的次序记录，不动引擎树；下一次 [`Layer::enforce`] 才把 root 子节点
    /// 顺序对齐（浮层通常是刚由 `App::create_overlay` 挂到 root 末尾的，所以这一帧往往已经
    /// 在正确位置，`enforce` 会零成本早退）。跨窗口登记的浮层不参与本窗口的槽位不变量。
    pub fn register(&self, app: &mut App, band: Band, overlay: EngineOverlay, modal: bool) -> u64 {
        self.register_with(app, band, overlay, modal, LayerSlot::Exclusive)
    }

    /// [`Layer::register`] 的槽位规则可变版本（子菜单用 `LayerSlot::Nested`）。
    pub(crate) fn register_with(
        &self,
        app: &mut App,
        band: Band,
        overlay: EngineOverlay,
        modal: bool,
        slot: LayerSlot,
    ) -> u64 {
        let window = overlay.window();
        if matches!(band, Band::Dialog | Band::Alert) {
            while self.top_of(window, Band::ContextMenu).is_some() {
                let _ = self.close_top_of(app, window, Band::ContextMenu);
            }
        }
        if slot == LayerSlot::Exclusive && matches!(band, Band::Alert | Band::ContextMenu) {
            while self.top_of(window, band).is_some() {
                let _ = self.close_top_of(app, window, band);
            }
        }
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

    /// 某窗口已登记层数。
    #[must_use]
    pub fn len(&self, window: WindowId) -> usize {
        self.inner
            .entries
            .borrow()
            .iter()
            .filter(|entry| entry.window() == window)
            .count()
    }

    /// 某窗口是否空栈。
    #[must_use]
    pub fn is_empty(&self, window: WindowId) -> bool {
        self.len(window) == 0
    }

    /// 某窗口的全部层，**自下而上**（层带升序，同带按登记序）。
    #[must_use]
    pub fn entries(&self, window: WindowId) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self
            .inner
            .entries
            .borrow()
            .iter()
            .copied()
            .filter(|entry| entry.window() == window)
            .collect();
        entries.sort_unstable_by_key(|entry| (entry.band, entry.sequence));
        entries
    }

    /// 某窗口的最上层（自下而上最后一项）。
    #[must_use]
    pub fn top(&self, window: WindowId) -> Option<Entry> {
        self.entries(window).pop()
    }

    /// 某窗口某层带的最上层。
    #[must_use]
    pub fn top_of(&self, window: WindowId, band: Band) -> Option<Entry> {
        self.entries(window)
            .into_iter()
            .rev()
            .find(|entry| entry.band == band)
    }

    /// 某窗口最上层**模态**层（焦点作用域根；`None` = 该窗口没有模态浮层）。
    ///
    /// 与引擎 `interaction::overlay::focus_scope_root` 同源：最上层带 `trap_focus` 的浮层说了算，
    /// 非模态层不改变焦点作用域。
    #[must_use]
    pub fn modal_scope(&self, window: WindowId) -> Option<Entry> {
        self.entries(window)
            .into_iter()
            .rev()
            .find(|entry| entry.modal)
    }

    /// 某窗口里 `node` 是否是某个已登记层的浮层节点。
    #[must_use]
    pub fn contains_node(&self, window: WindowId, node: NodeId) -> bool {
        self.entries(window)
            .iter()
            .any(|entry| entry.node() == node)
    }

    /// `overlay` 在某窗口当前栈里的高度序号（自下而上，`0` = 最底；未登记 ⇒ `None`）。
    #[must_use]
    pub fn position(&self, window: WindowId, overlay: &EngineOverlay) -> Option<usize> {
        self.entries(window)
            .iter()
            .position(|entry| entry.overlay == *overlay)
    }

    /// 把引擎 root 子节点顺序对齐到层带表；返回是否真的重排了（已对齐 ⇒ `false`，I3）。
    ///
    /// 判定只看**本窗口已登记浮层之间**的相对顺序：窗口里若有本栈未登记的浮层（理论上不应
    /// 存在，组合根应把所有浮层登记到同一个栈），它们的相对位置不参与判定；一旦需要重排，
    /// 已登记层会被整体提到它们之上。其它窗口的登记不参与判定、也不会被重排（R04）。
    ///
    /// 进入判定前先**剪除**引擎里已经不存在的登记（`overlays(window)` 里没有的节点）：外部
    /// 卸载（点外部 / Esc / 绕过 kit 直接 `close_overlay`）即使没走 {`Overlay::finish_close`}，
    /// 也不会让 `desired` 永远失配、把 `enforce` 变成每轮重排 + 出帧（R03 的防御面）。
    ///
    /// 重排用 [`ui_core::app::App::bring_overlay_to_front`]（把节点重挂到 root 末尾），按层带
    /// 自下而上依次调用，最终顺序即层带顺序。整条路径只在失配时发生，稳态零成本。
    pub fn enforce(&self, app: &mut App, window: WindowId) -> bool {
        let current: Vec<NodeId> = app
            .overlays(window)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        self.prune(window, &current);
        let desired: Vec<NodeId> = self
            .entries(window)
            .iter()
            .map(|entry| entry.node())
            .collect();
        if desired.is_empty() {
            return false;
        }
        let current_registered: Vec<NodeId> = current
            .into_iter()
            .filter(|node| desired.contains(node))
            .collect();
        if current_registered == desired {
            return false;
        }
        // 未登记浮层不参与排序：把它们留在原位，只把登记层按次序提到末尾。
        for entry in self.entries(window) {
            app.bring_overlay_to_front(&entry.overlay);
        }
        true
    }

    /// 关闭某窗口最上层（自下而上最后一项）；引擎负责恢复焦点。返回是否有层被关闭。
    pub fn close_top(&self, app: &mut App, window: WindowId) -> bool {
        let Some(entry) = self.top(window) else {
            return false;
        };
        let closed = app.close_overlay(&entry.overlay);
        self.remove(&entry.overlay);
        closed
    }

    /// 关闭某窗口某层带的最上层（旧「第二次 alert 替换第一次」/「新 context menu 替换旧的」
    /// 的通用入口）。返回引擎是否真的卸载了节点（登记已移除时仍可能返回 `false`）。
    pub fn close_top_of(&self, app: &mut App, window: WindowId, band: Band) -> bool {
        let Some(entry) = self.top_of(window, band) else {
            return false;
        };
        let closed = app.close_overlay(&entry.overlay);
        self.remove(&entry.overlay);
        closed
    }

    /// 剪除某窗口里引擎已经卸载的登记（只动该窗口，跨窗口登记不受影响）。
    fn prune(&self, window: WindowId, current: &[NodeId]) {
        self.inner
            .entries
            .borrow_mut()
            .retain(|entry| entry.window() != window || current.contains(&entry.node()));
    }
}

#[cfg(test)]
mod tests {
    //! 层带的**纯逻辑**断言：不需要窗口。需要引擎树的地方在 `ui_tests`。

    use super::Band;

    #[test]
    fn bands_order_from_dialog_to_tooltip() {
        let bands = [
            Band::Dialog,
            Band::Alert,
            Band::Popup,
            Band::ContextMenu,
            Band::Tooltip,
        ];
        for pair in bands.windows(2) {
            assert!(
                pair[0].order() < pair[1].order(),
                "{:?} 必须在 {:?} 之下",
                pair[0],
                pair[1]
            );
        }
        // 旧实现的固定关系（评估 G6）：alert 恒高于 dialog，context menu 高于 popup，tooltip 最高。
        assert!(Band::Alert.order() > Band::Dialog.order());
        assert!(Band::ContextMenu.order() > Band::Popup.order());
        assert!(Band::Tooltip.order() > Band::ContextMenu.order());
        assert!(Band::Popup.order() > Band::Alert.order());
        assert_eq!(Band::Dialog.order(), 10);
    }
}

#[cfg(test)]
mod ui_tests {
    //! 需要真实引擎树的断言：层带排序落到 root 子节点顺序、命中优先、模态作用域、
    //! 点击穿透、"关闭上层露出下层"、按窗口分区与旧槽位不变量。

    use ui_core::{
        event::{Action, InputEvent, Modifiers, PointerButton},
        interaction::{OverlayDismiss, OverlayOptions, OverlayPlacement},
        math::{Point, Size},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::{WindowId, WindowOptions},
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
                    ..OverlayOptions::default()
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

    fn node_exists(app: &TestApp, window: WindowId, node: NodeId) -> bool {
        app.app()
            .window(window)
            .expect("窗口")
            .tree()
            .get(node)
            .is_ok()
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
        layer.register(app.app_mut(), Band::Alert, alert, true);
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
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
        layer.register(app.app_mut(), Band::Dialog, first, false);
        layer.register(app.app_mut(), Band::Dialog, second, false);
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
        layer.register(app.app_mut(), Band::Dialog, lower, false);
        layer.register(app.app_mut(), Band::Popup, upper, false);
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
        // 下拉里的子菜单（ContextMenu）又在弹出层之上；提示（Tooltip）最高；
        // alert 恒高于 dialog（含后开的 dialog）。
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        // 故意用「子层先开、父层后开」的注册序制造失配。alert 先登记，避免它的槽位不变量
        // （打开 alert 先收起 context menu）影响本用例；dialog 后登记仍必须落在 alert 之下。
        let alert = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            true,
        );
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
        layer.register(app.app_mut(), Band::Alert, alert, true);
        layer.register(app.app_mut(), Band::Popup, popup, false);
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
        layer.register(app.app_mut(), Band::ContextMenu, submenu, false);
        layer.register(app.app_mut(), Band::Tooltip, tooltip, false);
        let _ = app.drain().expect("首帧");
        assert!(layer.enforce(app.app_mut(), window));
        let _ = app.drain().expect("帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![
                dialog.node(),
                alert.node(),
                popup.node(),
                submenu.node(),
                tooltip.node()
            ],
            "自下而上必须是 Dialog < Alert < Popup < ContextMenu < Tooltip"
        );
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(alert.node()),
            "焦点作用域根是最上层模态（alert 恒高于 dialog），弹出层不改变它"
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
        layer.register(app.app_mut(), Band::Dialog, lower, false);
        layer.register(app.app_mut(), Band::Dialog, upper, false);
        assert!(layer.remove(&lower));
        assert!(!layer.remove(&lower), "重复移除返回 false");
        assert_eq!(layer.len(window), 1);
        assert_eq!(
            layer.top(window).map(|entry| entry.node()),
            Some(upper.node())
        );
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
        layer.register(app.app_mut(), Band::Dialog, lower, true);
        layer.register(app.app_mut(), Band::Dialog, upper, true);
        let _ = app.drain().expect("首帧");

        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(upper.node())
        );
        assert!(layer.close_top(app.app_mut(), window));
        let _ = app.drain().expect("帧");

        assert_eq!(
            ordered_nodes(&app, window),
            vec![lower.node()],
            "关闭上层后只剩下层"
        );
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
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
        layer.register(app.app_mut(), Band::Alert, alert, true);
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
        layer.register(app.app_mut(), Band::ContextMenu, menu, false);
        let _ = app.drain().expect("首帧");

        assert!(
            layer.close_top_of(app.app_mut(), window, Band::Alert),
            "关掉 alert"
        );
        let _ = app.drain().expect("帧");
        assert_eq!(layer.len(window), 2);
        assert_eq!(layer.top_of(window, Band::Alert), None);
        assert!(layer.top_of(window, Band::Dialog).is_some());
        assert!(layer.top_of(window, Band::ContextMenu).is_some());
        // 定向关闭不动其它层的引擎节点，且被关的节点真的卸载了。
        assert!(
            !node_exists(&app, window, alert.node()),
            "被关闭的 alert 节点必须已卸载"
        );
        assert!(node_exists(&app, window, dialog.node()));
        assert!(node_exists(&app, window, menu.node()));
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
        layer.register(app.app_mut(), Band::Popup, popup, false);
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
        layer.register(app.app_mut(), Band::Dialog, modal, true);
        let _ = app.drain().expect("首帧");

        let at = center(&app, button_node);
        click(&mut app, at);
        let _ = app.drain().expect("帧");
        assert_eq!(
            *app.app().read(&clicks),
            0,
            "模态作用域外的按下不得触发下层动作"
        );
        assert!(layer.contains_node(window, modal.node()));
    }

    #[test]
    fn a_shared_layer_is_partitioned_by_window() {
        // R04：多窗口共用一个 Layer。窗口 A 的 enforce 只能看到 A 的登记；跨窗口的登记既不
        // 参与判定（否则 desired 永不相等 ⇒ 每轮重排 + 出帧），也不能被重排到另一个窗口。
        let (mut app, window_a, _root_a) = harness();
        let layer = Layer::new();
        let window_b = app.app_mut().create_window(WindowOptions::default());
        let alert_a = overlay_at(
            &mut app,
            window_a,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        let dialog_b = overlay_at(
            &mut app,
            window_b,
            Point::new(20.0, 20.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, alert_a, true);
        layer.register(app.app_mut(), Band::Dialog, dialog_b, true);
        let order_b = ordered_nodes(&app, window_b);

        assert_eq!(layer.len(window_a), 1);
        assert_eq!(layer.len(window_b), 1);
        assert!(
            !layer.enforce(app.app_mut(), window_a),
            "窗口 A 已对齐；窗口 B 的登记不参与 A 的判定（否则永不收敛）"
        );
        assert!(!layer.enforce(app.app_mut(), window_b), "窗口 B 同样已对齐");
        assert_eq!(
            ordered_nodes(&app, window_a),
            vec![alert_a.node()],
            "A 的 root 顺序不变"
        );
        assert_eq!(
            ordered_nodes(&app, window_b),
            order_b,
            "A 的 enforce 不得重排 B 的 root"
        );
        assert_eq!(
            layer.modal_scope(window_b).map(|entry| entry.node()),
            Some(dialog_b.node()),
            "modal_scope 按窗口分区"
        );
        assert_eq!(
            layer.modal_scope(window_a).map(|entry| entry.node()),
            Some(alert_a.node())
        );
    }

    #[test]
    fn enforce_prunes_entries_whose_overlay_was_closed_elsewhere() {
        // R03 防御面：绕过 kit 的引擎卸载（外部 `close_overlay` / 点外部 / Esc 后 sync 之前）
        // 不会让登记永远失配；下一轮 enforce 剪掉死登记并回到零工作。stale entry 也不能继续
        // 冒充 `modal_scope()`。
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let first = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        let second = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Dialog, first, true);
        layer.register(app.app_mut(), Band::Dialog, second, true);
        let _ = app.drain().expect("首帧");
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(second.node())
        );
        assert!(app.app_mut().close_overlay(&second), "引擎侧卸载最上层模态");
        assert_eq!(
            layer.len(window),
            2,
            "登记暂时还在（等下一轮 sync / enforce）"
        );

        assert!(
            !layer.enforce(app.app_mut(), window),
            "剪除死登记后已对齐 ⇒ 零工作（不得每轮重排）"
        );
        assert_eq!(layer.len(window), 1, "死登记必须被剪除");
        assert_eq!(
            layer.modal_scope(window).map(|entry| entry.node()),
            Some(first.node()),
            "modal_scope 必须回落到仍然存活的模态（不得对已死节点说谎）"
        );
        assert!(!layer.enforce(app.app_mut(), window), "此后稳态零工作");
    }

    #[test]
    fn registering_a_dialog_or_alert_dismisses_the_context_menu() {
        // 旧 layer.rs：dialog / alert 是模态的，打开前先收起 context menu（菜单不能压在模态
        // 之上继续吃按下）。
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, menu, false);
        let dialog = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Dialog, dialog, true);
        assert_eq!(layer.len(window), 1, "菜单被替换掉");
        assert!(layer.top_of(window, Band::ContextMenu).is_none());
        assert!(!node_exists(&app, window, menu.node()), "菜单节点已卸载");

        let menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, menu, false);
        let alert = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, alert, true);
        assert!(layer.top_of(window, Band::ContextMenu).is_none());
        assert!(
            !node_exists(&app, window, menu.node()),
            "alert 打开也收起菜单"
        );
    }

    #[test]
    fn a_second_alert_replaces_the_first_and_a_second_menu_replaces_the_first() {
        // 旧 layer.rs 的单例槽位：alert / context menu 至多一个，后开的替换先开的。
        let (mut app, window, _root) = harness();
        let layer = Layer::new();
        let first_alert = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, first_alert, true);
        let second_alert = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::Explicit,
            true,
        );
        layer.register(app.app_mut(), Band::Alert, second_alert, true);
        assert_eq!(
            layer.top_of(window, Band::Alert).map(|entry| entry.node()),
            Some(second_alert.node())
        );
        assert_eq!(layer.len(window), 1, "alert 槽位至多一个");
        assert!(
            !node_exists(&app, window, first_alert.node()),
            "旧 alert 已卸载"
        );

        let first_menu = overlay_at(
            &mut app,
            window,
            Point::ZERO,
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, first_menu, false);
        let second_menu = overlay_at(
            &mut app,
            window,
            Point::new(10.0, 10.0),
            OverlayDismiss::OutsideClick,
            false,
        );
        layer.register(app.app_mut(), Band::ContextMenu, second_menu, false);
        assert_eq!(layer.len(window), 2, "alert + 最新菜单");
        assert!(
            !node_exists(&app, window, first_menu.node()),
            "旧菜单已卸载"
        );
        assert!(node_exists(&app, window, second_menu.node()));
    }
}

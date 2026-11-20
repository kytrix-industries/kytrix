//! 通用交互机制（`custom-gui-engine-design.md` §3.6 / §3.7 / §8.4）。
//!
//! 本文件是**公共模块根**：契约类型定义在这里，实现放子模块（`focus` / `scroll` / `virtual_list` /
//! `overlay` / `drag` / `animation`）。每个类型只有**一个** `impl` 块（`multiple_inherent_impl`）。
//!
//! **状态归属（唯一真值）**：节点级槽 → `NodeInteraction`（挂在 `Node` 上）；焦点 / 指针 → `Window`；
//! handler 表 / 键绑定 / 动画槽 / 时钟 / 阈值 → `App`。

pub mod animation;
pub mod drag;
pub mod focus;
pub mod overlay;
pub mod scroll;
pub mod virtual_list;

use std::time::{Duration, Instant};

use crate::{
    app::App,
    event::{Action, ActionSource, Modifiers, PointerButton},
    layout::hit_test::HitResult,
    math::{Point, Rect, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{color::Color, style::Cursor},
    window::WindowId,
};

/// 一个窗口的焦点状态（**唯一**权威源：键盘路由、焦点环、a11y 的 `TreeUpdate.focus` 都读它）。
///
/// v1 只有一个作用域（单窗口）；Tab 序不缓存（`dfs_ids()` 过滤，见 [`crate::interaction::focus`]）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FocusScope {
    focused: Option<NodeId>,
}

impl FocusScope {
    /// 当前焦点（`None` = 无焦点）。
    #[must_use]
    pub fn focused(&self) -> Option<NodeId> {
        self.focused
    }

    /// 该节点是否有焦点。
    #[must_use]
    pub fn is_focused(&self, node: NodeId) -> bool {
        self.focused == Some(node)
    }

    /// 改焦点；返回是否真的变了（同值 ⇒ `false`，调用方据此跳过标脏）。
    pub(crate) fn set(&mut self, node: Option<NodeId>) -> bool {
        if self.focused == node {
            return false;
        }
        self.focused = node;
        true
    }
}

/// 一次「指针按下」记录（点击判定与拖拽抑制的输入）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PressRecord {
    /// 按下时的命中节点（`None` = 点在空白处）。
    pub node: Option<NodeId>,
    /// 按下位置（逻辑像素）。
    pub position: Point,
    /// 按下的按钮。
    pub button: PointerButton,
    /// 按下时刻。
    pub at: Instant,
}

/// 一次拖拽会话。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DragSession {
    /// 声明 `on_drag` 的节点。
    pub node: NodeId,
    /// 该节点声明的动作。
    pub action: Action,
    /// 拖拽起点（逻辑像素）。
    pub origin: Point,
    /// 当前位置（逻辑像素）。
    pub current: Point,
    /// 最近一次派发的阶段（`App::drag` 的只读面用）。
    pub phase: DragPhase,
}

/// 「已按下、但位移还没超过阈值」的拖拽候选（R02-9）。
///
/// 按下时只登记候选，**不**派发 [`DragPhase::Start`]；指针移动首次越过
/// [`InputThresholds::drag_distance`] 时才转成 [`DragSession`] 并派发 `Start`（delta 用真实
/// 位移）。这正是 Qt（`QApplication::startDragDistance()` 之后才 `QDrag`）、GTK
/// （`gtk_drag_check_threshold`）、Blink（`mousemove` 超过阈值才 `dragstart`）、GPUI
/// （`on_drag` 在 `moved` 超阈值后触发）的仲裁方式：**拖拽 vs 点击由引擎裁决**，应用不必
/// 自己实现同一套状态机，也不会在「假拖拽」里先把状态改掉却无法撤回。
///
/// **只有主键**登记候选（R02-3）：次键按下不改变交互状态；拖拽会话也只由主键抬起结束。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingDrag {
    /// 声明 `on_drag` 的节点。
    pub node: NodeId,
    /// 该节点声明的动作。
    pub action: Action,
    /// 按下位置（阈值判定的基准）。
    pub origin: Point,
}

impl DragSession {
    /// 相对起点的位移。
    #[must_use]
    pub fn delta(&self) -> Point {
        Point::new(
            self.current.x - self.origin.x,
            self.current.y - self.origin.y,
        )
    }
}

/// 拖拽阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase {
    /// 拖拽**成立**：指针移动首次越过 [`InputThresholds::drag_distance`] 时派发，`delta` 是
    /// 真实位移（不再是 0）。按下本身不派发本阶段（R02-9）。
    Start,
    /// 指针移动。
    Move,
    /// 抬起结束（位移已超过阈值）。
    End,
    /// 失焦 / 显式取消。
    Cancel,
}

/// 拖拽的只读面（[`crate::app::App::drag`] 的返回值）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragInfo {
    /// 拖拽起点。
    pub origin: Point,
    /// 当前位置。
    pub current: Point,
    /// 位移。
    pub delta: Point,
    /// 最近一次派发的阶段。
    pub phase: DragPhase,
}

/// 拖拽会话句柄（`Copy`；节点卸载后读取返回 `None`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DragState {
    window: WindowId,
    node: NodeId,
}

impl DragState {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// 声明 `on_drag` 的节点。
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

/// 窗口的指针状态（hover / 光标 / 按下 / 拖拽 / 捕获）。
///
/// `pub(crate)` 结构 + `pub` 字段（P06 `PaintPass` 的同一写法；`field_scoped_visibility_modifiers` 不允许
/// 字段级 `pub(crate)`）。
#[derive(Clone, Debug)]
pub(crate) struct PointerState {
    /// 光标下的节点（`None` = 空白处或窗口外）；公开读点是
    /// [`crate::app::App::hovered`]（R02-4）。
    pub hovered: Option<NodeId>,
    /// 当前解析出的光标（只有变化时才推给平台）。
    pub cursor: Cursor,
    /// 最近一次按下的记录。
    pub press: Option<PressRecord>,
    /// 已按下但还没越过拖拽阈值的候选（R02-9）。
    pub pending_drag: Option<PendingDrag>,
    /// 正在进行的拖拽会话。
    pub drag: Option<DragSession>,
    /// 指针捕获的目标节点（`None` = 没有捕获；R02-8）。
    ///
    /// 语义是**平台级捕获**（W3C `setPointerCapture`、Qt `QWidget::grabMouse` 的对应物）：
    /// 让系统继续把指针事件投递给本窗口（Windows `SetCapture`；macOS / Wayland 由系统在按下
    /// 期间隐式保证），引擎**不**做「命名节点路由」—— 事件仍按指针位置命中，`ActionCtx::hit`
    /// 在窗口外是 `None`。捕获目标被卸载时 `remove_node` 会释放捕获并派发 `PointerCancel`，
    /// [`crate::app::App::pointer_captured`] 也会过滤陈旧 id。
    pub capture: Option<NodeId>,
    /// 最近一次命中的原始结果（`ActionCtx::hit` 的来源）。
    pub last_hit: Option<HitResult>,
    /// 最近一次输入事件的修饰键（`ActionCtx::modifiers` 的来源）。
    pub modifiers: Modifiers,
}

impl PointerState {
    pub(crate) fn new() -> Self {
        Self {
            hovered: None,
            cursor: Cursor::Default,
            press: None,
            pending_drag: None,
            drag: None,
            capture: None,
            last_hit: None,
            modifiers: Modifiers::empty(),
        }
    }

    /// 结束拖拽并释放捕获；返回是否真的结束了一个会话。
    pub(crate) fn end_drag(&mut self) -> Option<DragSession> {
        self.capture = None;
        self.pending_drag = None;
        self.drag.take()
    }
}

/// 滚动轴（**容器是单轴的**；R02-11 的取舍）。
///
/// 引擎的滚动容器只声明**一条**轴：纵向列表、横向图表各自一个容器。二维 `delta`
/// （[`crate::event::InputEvent::Scroll`]）按轴拆分后沿滚动链逐层消费：内层容器只吃自己
/// 声明的轴，另一轴原样传给外层（浏览器的 scroll chaining）。
///
/// **需要同时平移两轴的区域由两层容器表达**：内层 [`ScrollAxis::Horizontal`] + 外层
/// [`ScrollAxis::Vertical`]，每轴各由一个容器消费（K 线图 / 深度图的双指平移就是这样做的）。
/// 「单个容器同时吃两轴」不在本引擎的契约内 —— 旧文档只写了「二维 delta 必须可达」却没写
/// 消费方式，属于半截语义（AR1 §2-11）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAxis {
    /// 垂直滚动。
    Vertical,
    /// 水平滚动。
    Horizontal,
}

/// 滚动容器的构造参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollConfig {
    /// 轴（单轴，见 [`ScrollAxis`]）。
    pub axis: ScrollAxis,
    /// 一格滚轮对应的逻辑像素（默认 48）。
    pub line_height: f32,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            axis: ScrollAxis::Vertical,
            line_height: 48.0,
        }
    }
}

/// 滚动容器的句柄（`Copy`；带窗口，跨窗口的 `NodeId` 会假命中）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScrollHandle {
    window: WindowId,
    node: NodeId,
}

impl ScrollHandle {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// 滚动容器节点。
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

/// 滚动状态快照（应用读偏移的唯一公开面；读的是 tree 侧槽，不存在第二份）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollState {
    /// 当前偏移（逻辑像素，内容坐标 → 屏幕坐标的位移量）。**另一轴恒为 0**（容器单轴）。
    pub offset: Point,
    /// 内容尺寸（应用 / 虚拟列表显式给）。
    pub content: Size,
    /// 视口尺寸（`bounds.inset(border)`，现算）。
    pub viewport: Size,
    /// 轴（单轴容器：另一轴的可用范围恒 0）。
    pub axis: ScrollAxis,
}

/// 滚动槽（`Node.interaction.scroll`）。
///
/// 单轴：`offset` 只有 [`ScrollSlot::axis`] 那一条轴可能非零，`max_offset` / `clamp_offset`
/// 把另一轴强制为 0（写另一轴会被静默夹掉）。二维 `delta` 的跨轴外传在
/// [`crate::app::input::on_scroll`] 的滚动链里完成。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollSlot {
    /// 当前偏移（已 clamp）。
    pub offset: Point,
    /// 内容尺寸。
    pub content: Size,
    /// 轴。
    pub axis: ScrollAxis,
    /// 一格滚轮的逻辑像素。
    pub line_height: f32,
}

/// 虚拟列表容器的句柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VirtualList {
    window: WindowId,
    node: NodeId,
}

impl VirtualList {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// 容器节点（同时是滚动容器）。
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

/// 虚拟列表参数（v2.1 / E3：可选逐项高度）。
///
/// `extents = None` ⇒ 与 v1 逐字节一致（行位置 `index * extent`、内容高度 `count * extent`）；
/// `extents = Some(..)` ⇒ 逐项高度，`extent` 仅作为条目数变化后新高度尚未送达时的回落值。
///
/// 创建时就已知行高时可在 `make_virtual_list` 里给 `extents`：引擎先记住它，`set_virtual_items`
/// 的条目数与之匹配时生效；不匹配则丢弃（应用随后用 [`crate::app::App::set_virtual_extents`]
/// 整体替换）。
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualListConfig {
    /// 每行高度（逻辑像素，`> 0`）。
    pub extent: f32,
    /// 逐项高度（逻辑像素，每项有限且 `>= 0`，累计高度必须有限）：`Some` 时以它为准
    /// （条目数变化丢弃后回落到 `extent`）。
    pub extents: Option<Box<[f32]>>,
    /// 视口外多挂几行（上下一侧各 `overscan`）。
    pub overscan: usize,
}

/// `scroll_to_item` 的对齐方式（E3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemAlign {
    /// 条目顶边贴视口顶边。
    Start,
    /// 条目中心对齐视口中心。
    Center,
    /// 条目底边贴视口底边。
    End,
    /// 已完全可见则不滚，否则滚到最近的边界。
    Nearest,
}

/// 虚拟列表状态快照（也供 a11y 计算 `pos_in_set` / `set_size`）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VirtualListState {
    /// 条目总数。
    pub count: usize,
    /// 当前窗口的首个条目下标。
    pub first: usize,
    /// 当前窗口的尾后下标。
    pub last: usize,
    /// 已挂载的行数。
    pub mounted: usize,
    /// 当前偏移。
    pub offset: Point,
    /// 视口尺寸。
    pub viewport: Size,
}

/// 虚拟列表槽（`Node.interaction.virtual_list`）。
pub(crate) struct VirtualSlot {
    /// 行高（`extents = None` 或逐项高度尚未与条目数对齐时的回落值）。
    pub extent: f32,
    /// 逐项高度（E3）。`Some` 时 `prefix` 同步可用。
    pub extents: Option<Box<[f32]>>,
    /// `extents` 的前缀和缓存（`len == extents.len() + 1`、`prefix[0] = 0`）。
    ///
    /// **只在 `set_extents` / `clear_extents` 里重建**：滚动帧只做二分查询（O(log n)），
    /// 不每帧重算（I3 的预算前提）。
    prefix: Vec<f32>,
    /// overscan。
    pub overscan: usize,
    /// 条目总数。
    pub count: usize,
    /// 行工厂（`set_virtual_items` 写入；挂载时按需调用）。
    pub builder: Option<Box<dyn FnMut(usize) -> NodeBuilder>>,
    /// 已挂载的 `(index, row)`（按 index 升序）。
    pub mounted: Vec<(usize, NodeId)>,
}

impl VirtualSlot {
    /// 空槽（`make_virtual_list` 的唯一构造点；`prefix` 只随 [`VirtualSlot::set_extents`] 变化）。
    pub(crate) fn new(extent: f32, overscan: usize) -> Self {
        Self {
            extent,
            extents: None,
            prefix: Vec::new(),
            overscan,
            count: 0,
            builder: None,
            mounted: Vec::new(),
        }
    }

    /// 整体替换逐项高度并重算前缀和（O(n)）。
    pub(crate) fn set_extents(&mut self, extents: Box<[f32]>) {
        self.prefix.clear();
        self.prefix.reserve(extents.len() + 1);
        self.prefix.push(0.0);
        let mut total = 0.0;
        for &height in &extents {
            total += height;
            self.prefix.push(total);
        }
        self.extents = Some(extents);
    }

    /// 丢弃逐项高度，回落到单一 [`VirtualSlot::extent`]。
    pub(crate) fn clear_extents(&mut self) {
        self.extents = None;
        self.prefix.clear();
    }

    /// 当前**生效**的逐项高度：`Some` ⇔ 长度与 [`VirtualSlot::count`] 一致。
    ///
    /// 不一致只出现在「`make_virtual_list` 预置高度 → `set_virtual_items`」之间：那段时间
    /// 没有行、也没有帧，按单一 `extent` 处理。
    pub(crate) fn active_extents(&self) -> Option<&[f32]> {
        self.extents
            .as_deref()
            .filter(|extents| extents.len() == self.count)
    }

    /// 前缀和缓存（长度 `count + 1`；仅当 [`VirtualSlot::active_extents`] 为 `Some` 时可用）。
    pub(crate) fn prefix(&self) -> &[f32] {
        &self.prefix
    }
}

impl std::fmt::Debug for VirtualSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualSlot")
            .field("extent", &self.extent)
            .field("extents", &self.extents.as_deref().map(<[f32]>::len))
            .field("prefix", &self.prefix.len())
            .field("overscan", &self.overscan)
            .field("count", &self.count)
            .field("builder", &self.builder.as_ref().map(|_| "<closure>"))
            .field("mounted", &self.mounted)
            .finish()
    }
}

/// 浮层的关闭策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayDismiss {
    /// 点击浮层外部即关闭（并消费该次按下）。
    OutsideClick,
    /// 只能显式关闭（[`crate::app::App::close_overlay`] 或 `Dismiss` 动作）。
    Explicit,
}

/// 浮层相对锚点的方位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorSide {
    /// 锚点下方。
    Below,
    /// 锚点上方。
    Above,
    /// 锚点左侧。
    Left,
    /// 锚点右侧。
    Right,
}

/// 浮层定位（打开时求值一次；移动锚点需再 [`crate::app::App::place_overlay`]）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayPlacement {
    /// 窗口根的逻辑像素点（浮层左上角）。
    Point(Point),
    /// 锚定到某节点的某一侧。
    Anchor {
        /// 锚点节点。
        node: NodeId,
        /// 方位。
        side: AnchorSide,
        /// 间隙（逻辑像素）。
        gap: f32,
    },
}

/// 浮层参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayOptions {
    /// 定位。
    pub placement: OverlayPlacement,
    /// 关闭策略。
    pub dismiss: OverlayDismiss,
    /// 是否**模态**（R02-12）：把键盘焦点作用域限制在浮层子树内。
    ///
    /// 置位时（对应 Blink `<dialog>.showModal()` 的 top layer + `inert` 背景、Qt
    /// `QDialog` 的对话框焦点作用域、GTK `GtkWindow` 的 modal）：
    /// - Tab / Shift+Tab 只在作用域内循环（含**属于该模态的弹出层**）；
    /// - [`crate::app::App::set_focus`] 拒绝作用域外的节点；
    /// - 浮层外的指针按下 / 滚动被**消费**（不抢焦点、不触发下层动作、不滚背景）；
    /// - Enter / Space 激活与 a11y 动作（`ActionRequest`，**全部**动作类型）拒绝作用域外目标；
    ///   注意作用域只约束**动作**：a11y 树本身仍向外暴露背景节点（读屏可以读到它们，只是
    ///   操作会被拒绝）—— 把背景置 `inert`（accesskit `hidden`）尚未实现（AR1b N2）；
    /// - 作用域外的焦点在下一次输入/焦点操作时移入模态内第一个可聚焦节点（子节点在
    ///   `create_overlay` 返回之后才挂载，创建瞬间求值不到候选）；**完全没有焦点**时同样
    ///   补一个落点（AR1b N1b）。
    ///
    /// **作用域 ≠ 模态子树**：模态之上打开的弹出层（下拉/菜单/子菜单）只要 `opener` 在模态
    /// 内（可嵌套递归）就仍属于作用域（[`crate::interaction::overlay::in_modal_scope`]）。
    /// 这是 Qt 模态对话框的「子窗口/弹窗归属」与 Chrome top layer 的同一语义；旧实现只认
    /// 子树，于是对话框里的下拉菜单永远点不动（AR1 探针 1）。
    pub trap_focus: bool,
    /// 关闭时是否把焦点还给**打开者**（R02-12）。
    ///
    /// 打开时记录当时的焦点节点，关闭（[`crate::app::App::close_overlay`] / `Dismiss` 动作 /
    /// 外部点击）后若该节点仍存在且可聚焦就恢复。平台与 a11y 的惯例是「焦点回到 opener」：
    /// Blink 的 `<dialog>` 关闭、Qt `QDialog::done`、GTK `GtkPopover` 的 `autohide` 都这样做。
    pub restore_focus: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            placement: OverlayPlacement::Point(Point::ZERO),
            dismiss: OverlayDismiss::OutsideClick,
            trap_focus: false,
            restore_focus: false,
        }
    }
}

/// 浮层句柄（`Copy`；浮层永远是 root 的子节点 ⇒ 在 DFS 先序末尾，命中优先）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Overlay {
    window: WindowId,
    node: NodeId,
}

impl Overlay {
    pub(crate) fn new(window: WindowId, node: NodeId) -> Self {
        Self { window, node }
    }

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// 浮层节点。
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }
}

/// 浮层槽（`Node.interaction.overlay`）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OverlaySlot {
    /// 关闭策略。
    pub dismiss: OverlayDismiss,
    /// 打开时的定位：布局之后要按浮层**实测尺寸**重新求值（`Above` / `Left` 依赖它）。
    pub placement: OverlayPlacement,
    /// 是否模态（焦点作用域，R02-12）。
    pub trap_focus: bool,
    /// 打开时的焦点节点（关闭时恢复，R02-12）。
    pub opener: Option<NodeId>,
    /// 关闭时是否把焦点还给 `opener`。
    pub restore_focus: bool,
}

/// 动画句柄（`Copy`；`index` 带代际，取消/结束后旧句柄失效）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnimationId {
    window: WindowId,
    /// 单调代际（`App::cancel_animation` 只认这一对 `(window, generation)`）。
    generation: u32,
}

impl AnimationId {
    pub(crate) fn new(window: WindowId, generation: u32) -> Self {
        Self { window, generation }
    }

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }
}

/// 缓动曲线。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Easing {
    /// 线性。
    Linear,
    /// `1 - (1 - t)^3`。
    EaseOutCubic,
}

impl Easing {
    /// 采样（`t` 已 clamp 到 `[0, 1]`；`t == 1.0` 时**精确**返回 `1.0`，不靠浮点收敛）。
    #[must_use]
    pub fn sample(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        if t >= 1.0 {
            return 1.0;
        }
        match self {
            Self::Linear => t,
            Self::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
        }
    }
}

/// 动画的时间参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationSpec {
    /// 时长（`Duration::ZERO` = 立即到位，不建槽）。
    pub duration: Duration,
    /// 缓动。
    pub easing: Easing,
}

impl AnimationSpec {
    /// 默认的「短过渡」（120 ms，`EaseOutCubic`）。
    #[must_use]
    pub fn short() -> Self {
        Self {
            duration: Duration::from_millis(120),
            easing: Easing::EaseOutCubic,
        }
    }
}

/// 被动画驱动的值（`Transient` 是唯一落点：不进依赖图，`bind_style` 用 `read_transient` 读）。
#[derive(Clone, Debug)]
pub enum AnimatedValue {
    /// 标量（展开高度 / 透明度 / 进度）。
    Scalar {
        /// 目标瞬态值。
        target: crate::node::signal::Transient<f32>,
        /// 终值。
        to: f32,
    },
    /// 颜色（选中高亮 / 禁用过渡）。
    Color {
        /// 目标瞬态值。
        target: crate::node::signal::Transient<Color>,
        /// 终值。
        to: Color,
    },
}

/// 输入判定的系统阈值（默认值 = 各平台的系统缺省；平台在 `run()` 覆盖为系统真值）。
///
/// R02-15：阈值是**用户偏好**而不是常数。三个平台都必须有可用的默认值，并且都必须在
/// `run()` 里按同一形态覆盖成系统真值：
///
/// | 字段 | Win32 | AppKit | Wayland/GTK |
/// |---|---|---|---|
/// | `double_click_time` | `GetDoubleClickTime()` | `+[NSEvent doubleClickInterval]` | **无系统查询**（用平台缺省） |
/// | `double_click_distance` | `SM_CXDOUBLECLK`/`SM_CYDOUBLECLK` | **无系统查询**（用平台缺省） | **无系统查询**（用平台缺省） |
/// | `drag_distance` | `SM_CXDRAG`/`SM_CYDRAG` | **无系统查询**（用平台缺省） | **无系统查询**（用平台缺省） |
///
/// **Linux 没有系统查询通路**（XSettings / GTK 的 `gtk-double-click-time` 需要额外依赖，
/// 本引擎不引入）：`platform_default()` 的 400 ms 是 GTK 250 ms 与 Qt 400 ms 之间的稳定
/// 取值，引擎**静默**使用它 —— 这里明确声明「Linux 不读系统设置」，不留「会留一条诊断」
/// 之类的空承诺。macOS 同理只有双击间隔可读。
///
/// [`Default`] 实现与 [`InputThresholds::platform_default`] **完全一致**：应用自己构造
/// `InputThresholds::default()` 得到的值与引擎缺省相同（旧实现 `Default` 恒 500 ms，与
/// Linux 的 400 ms 分叉）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputThresholds {
    /// 双击的时间窗口（系统缺省 500 ms；Linux 400 ms）。
    pub double_click_time: Duration,
    /// 双击的位置容差（系统缺省 4 px；X/Y 取同一个值）。
    pub double_click_distance: f32,
    /// 「拖动 vs 点击」的位移阈值（系统缺省 4 px）。
    ///
    /// 两处语义**共用**同一个阈值：越过它 ⇒ 抑制 click（点击仲裁）且拖拽会话转正
    /// （`DragPhase::Start`）。旧实现把同一阈值只用于抑制 click、拖拽按下即 Start，
    /// 二者行为不一致（R02-9）。
    pub drag_distance: f32,
}

impl Default for InputThresholds {
    fn default() -> Self {
        Self::platform_default()
    }
}

impl InputThresholds {
    /// 平台缺省（平台读系统值失败时保留它；[`Default`] 直接返回它）。
    ///
    /// 数值来源：Win32 的 `GetDoubleClickTime()` 缺省 500 ms、`SM_CXDRAG`/`SM_CYDRAG` 缺省
    /// 4 px；AppKit 的 `+[NSEvent doubleClickInterval]` 缺省 0.5 s；**Linux 没有系统查询**
    /// （本引擎不引入 XSettings / GTK 依赖），取 GTK `gtk-double-click-time` 250 ms 与
    /// Qt `QStyleHints::mouseDoubleClickInterval` 400 ms 之间的 400 ms —— 这是没有
    /// 系统通路时唯一稳定的选择，且与 `Default` 一致。
    #[must_use]
    pub const fn platform_default() -> Self {
        #[cfg(windows)]
        {
            Self {
                double_click_time: Duration::from_millis(500),
                double_click_distance: 4.0,
                drag_distance: 4.0,
            }
        }
        #[cfg(target_os = "macos")]
        {
            Self {
                double_click_time: Duration::from_millis(500),
                double_click_distance: 4.0,
                drag_distance: 4.0,
            }
        }
        #[cfg(target_os = "linux")]
        {
            Self {
                double_click_time: Duration::from_millis(400),
                double_click_distance: 4.0,
                drag_distance: 4.0,
            }
        }
    }
}

/// 动作分发的上下文（**唯一**给应用的交互入口）。
pub struct ActionCtx<'a> {
    app: &'a mut App,
    window: WindowId,
    node: NodeId,
    hit: Option<HitResult>,
    modifiers: Modifiers,
    source: ActionSource,
}

impl<'a> ActionCtx<'a> {
    pub(crate) fn new(
        app: &'a mut App,
        window: WindowId,
        node: NodeId,
        hit: Option<HitResult>,
        modifiers: Modifiers,
        source: ActionSource,
    ) -> Self {
        Self {
            app,
            window,
            node,
            hit,
            modifiers,
            source,
        }
    }

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// 声明动作的节点（键盘路径 = 焦点节点）。
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// 原始命中（指针事件；键盘为 `None`）。
    #[must_use]
    pub fn hit(&self) -> Option<HitResult> {
        self.hit
    }

    /// 触发时的修饰键。
    #[must_use]
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// 来源（点击 / 键盘 / 拖拽 / 程序）。
    #[must_use]
    pub fn source(&self) -> ActionSource {
        self.source
    }

    /// **非追踪**读（handler 不是渲染 pass，不登记依赖），返回读守卫（零拷贝）。
    ///
    /// 守卫不借用 `ActionCtx`；在它存活期间对**同一个**信号的写会 panic（借用规则见
    /// `node::signal` 模块文档）。需要长期持有旧值时用 `(*guard).clone()`。
    pub fn read<T: 'static>(
        &self,
        signal: &crate::node::signal::Signal<T>,
    ) -> crate::node::signal::SignalRef<T> {
        self.app.read(signal)
    }

    /// 写状态（`App::write` 的转调：等值短路 + `dependents` 反查 + 唤醒）。
    ///
    /// # Panics
    /// 该信号存在活跃读守卫。
    pub fn write<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        value: T,
    ) -> bool {
        self.app.write(signal, value)
    }

    /// 原地更新状态（`App::update` 的转调：无等值短路）。
    ///
    /// # Panics
    /// 该信号存在活跃读守卫。
    pub fn update<T: 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        update: impl FnOnce(&mut T),
    ) -> bool {
        self.app.update(signal, update)
    }

    /// 其余交互能力（滚动 / 焦点 / 浮层 / 拖拽）走 `&mut App`。
    pub fn app_mut(&mut self) -> &mut App {
        self.app
    }
}

/// 节点级交互槽（`Node.interaction`；`pub(crate)` 结构 + `pub` 字段）。
///
/// 一个交互节点只付**一次**分配（`Option<Box<NodeInteraction>>` = 8 B/节点）。
pub(crate) struct NodeInteraction {
    /// 点击 / 激活声明的动作（`on_click` / `on_action` 写同一槽）。
    pub action: Option<Action>,
    /// 拖拽声明的动作（`on_drag`）。
    pub drag: Option<Action>,
    /// 滚动槽。
    pub scroll: Option<ScrollSlot>,
    /// 虚拟列表槽。
    pub virtual_list: Option<VirtualSlot>,
    /// 浮层槽。
    pub overlay: Option<OverlaySlot>,
    /// 焦点环（`NodeBuilder::focus_ring`；paint 在焦点节点自己的 lane 内画）。
    pub focus_ring: Option<crate::paint::style::FocusRing>,
}

impl NodeInteraction {
    pub(crate) fn new() -> Self {
        Self {
            action: None,
            drag: None,
            scroll: None,
            virtual_list: None,
            overlay: None,
            focus_ring: None,
        }
    }
}

impl std::fmt::Debug for NodeInteraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeInteraction")
            .field("action", &self.action)
            .field("drag", &self.drag)
            .field("scroll", &self.scroll)
            .field("virtual_list", &self.virtual_list)
            .field("overlay", &self.overlay)
            .field("focus_ring", &self.focus_ring)
            .finish()
    }
}

/// 浮层定位求值：把 [`OverlayPlacement`] 解成窗口绝对的左上角。
///
/// `Anchor` 读锚点的**屏幕**布局盒（内容坐标减去祖先滚动偏移）：浮层挂在 root 上，必须与屏幕上看到的
/// 锚点对齐。
///
/// `size` 是浮层**自身**的测量尺寸：`Above` / `Left` 要把浮层的下/右边缘贴到锚点外侧，
/// 必须减去自身尺寸（P2-6）。旧实现返回锚点的上/左边缘，浮层会盖住锚点。首帧尺寸未知时
/// 传 `Size::ZERO`，`App` 在布局之后会自动重新定位一次。
pub(crate) fn resolve_placement(
    tree: &crate::node::tree::NodeTree,
    placement: OverlayPlacement,
    size: crate::math::Size,
) -> Point {
    match placement {
        OverlayPlacement::Point(origin) => origin,
        OverlayPlacement::Anchor { node, side, gap } => {
            let bounds = tree.get(node).map_or_else(
                |_| Rect::default(),
                |target| scroll::to_screen(tree, node, target.layout().bounds),
            );
            match side {
                AnchorSide::Below => Point::new(bounds.origin.x, bounds.max().y + gap),
                AnchorSide::Above => {
                    Point::new(bounds.origin.x, bounds.origin.y - gap - size.height)
                }
                AnchorSide::Left => Point::new(bounds.origin.x - gap - size.width, bounds.origin.y),
                AnchorSide::Right => Point::new(bounds.max().x + gap, bounds.origin.y),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Easing, FocusScope, InputThresholds, ScrollAxis, ScrollConfig, ScrollSlot};
    use crate::{math::Point, node::id::NodeId};

    #[test]
    fn focus_scope_reports_and_dedupes() {
        let mut scope = FocusScope::default();
        let node = NodeId::for_test(1, 1);

        assert_eq!(scope.focused(), None);
        assert!(scope.set(Some(node)));
        assert_eq!(scope.focused(), Some(node));
        assert!(scope.is_focused(node));
        assert!(!scope.set(Some(node)), "同值必须返回 false（不标脏）");
        assert!(scope.set(None));
        assert_eq!(scope.focused(), None);
    }

    #[test]
    fn easing_is_monotonic_and_exact_at_the_end() {
        let mut last = -1.0;
        for step in 0..=10 {
            let value = Easing::EaseOutCubic.sample(step as f32 / 10.0);
            assert!(value >= last, "缓动必须单调");
            assert!((0.0..=1.0).contains(&value));
            last = value;
        }
        assert_eq!(Easing::EaseOutCubic.sample(1.0), 1.0);
        assert_eq!(Easing::Linear.sample(0.5), 0.5);
    }

    /// R02-15：`Default` 与 `platform_default()` 必须同值（旧实现 `Default` 恒 500 ms，
    /// 与 Linux 的 400 ms 分叉 —— 应用自己构造的阈值和引擎缺省不一样）。
    #[test]
    fn thresholds_default_matches_the_platform_defaults() {
        let thresholds = InputThresholds::default();
        let platform = InputThresholds::platform_default();

        assert_eq!(thresholds, platform);
        assert_eq!(thresholds.double_click_distance, 4.0);
        assert_eq!(thresholds.drag_distance, 4.0);
        // 双击窗口是平台相关值：Windows/macOS 500 ms、Linux 400 ms（无系统查询通路）。
        assert!(
            matches!(thresholds.double_click_time.as_millis(), 400 | 500),
            "双击窗口必须是平台缺省之一（实际 {:?}）",
            thresholds.double_click_time
        );
    }

    #[test]
    fn scroll_config_defaults_to_vertical() {
        let config = ScrollConfig::default();
        let slot = ScrollSlot {
            offset: Point::ZERO,
            content: crate::math::Size::new(100.0, 200.0),
            axis: config.axis,
            line_height: config.line_height,
        };

        assert_eq!(config.axis, ScrollAxis::Vertical);
        assert_eq!(config.line_height, 48.0);
        assert_eq!(slot.offset, Point::ZERO);
    }
}

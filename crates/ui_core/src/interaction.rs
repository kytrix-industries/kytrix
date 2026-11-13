//! 通用交互机制（`custom-gui-engine-design.md` §3.6 / §3.7 / §8.4；`design/phase-10-interaction.md`）。
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
/// v1 只有一个作用域（单窗口）；Tab 序不缓存（`order()` 过滤，见 [`crate::interaction::focus`]）。
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
    /// 按下即开始（`delta == 0`）。
    Start,
    /// 指针移动。
    Move,
    /// 抬起结束。
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
    /// 光标下的节点（`None` = 空白处或窗口外）。
    pub hover: Option<NodeId>,
    /// 当前解析出的光标（只有变化时才推给平台）。
    pub cursor: Cursor,
    /// 最近一次按下的记录。
    pub press: Option<PressRecord>,
    /// 正在进行的拖拽会话。
    pub drag: Option<DragSession>,
    /// 是否请求了指针捕获（拖拽期间）。
    pub capture: bool,
    /// 最近一次命中的原始结果（`ActionCtx::hit` 的来源）。
    pub last_hit: Option<HitResult>,
    /// 最近一次输入事件的修饰键（`ActionCtx::modifiers` 的来源）。
    pub modifiers: Modifiers,
}

impl PointerState {
    pub(crate) fn new() -> Self {
        Self {
            hover: None,
            cursor: Cursor::Default,
            press: None,
            drag: None,
            capture: false,
            last_hit: None,
            modifiers: Modifiers::empty(),
        }
    }

    /// 结束拖拽并释放捕获；返回是否真的结束了一个会话。
    pub(crate) fn end_drag(&mut self) -> Option<DragSession> {
        self.capture = false;
        self.drag.take()
    }
}

/// 滚动轴（v1 单轴）。
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
    /// 轴。
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
    /// 当前偏移（逻辑像素，内容坐标 → 屏幕坐标的位移量）。
    pub offset: Point,
    /// 内容尺寸（应用 / 虚拟列表显式给）。
    pub content: Size,
    /// 视口尺寸（`bounds.inset(border)`，现算）。
    pub viewport: Size,
    /// 轴。
    pub axis: ScrollAxis,
}

/// 滚动槽（`Node.interaction.scroll`）。
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

/// 虚拟列表参数（v1 只支持均匀行高）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VirtualListConfig {
    /// 每行高度（逻辑像素，`> 0`）。
    pub extent: f32,
    /// 视口外多挂几行（上下一侧各 `overscan`）。
    pub overscan: usize,
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
    /// 行高。
    pub extent: f32,
    /// overscan。
    pub overscan: usize,
    /// 条目总数。
    pub count: usize,
    /// 行工厂（`set_virtual_items` 写入；挂载时按需调用）。
    pub builder: Option<Box<dyn FnMut(usize) -> NodeBuilder>>,
    /// 已挂载的 `(index, row)`（按 index 升序）。
    pub mounted: Vec<(usize, NodeId)>,
}

impl std::fmt::Debug for VirtualSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualSlot")
            .field("extent", &self.extent)
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
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            placement: OverlayPlacement::Point(Point::ZERO),
            dismiss: OverlayDismiss::OutsideClick,
        }
    }
}

/// 浮层句柄（`Copy`；浮层永远是 root 的子节点 ⇒ 在 `order` 末尾，命中优先）。
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverlaySlot {
    /// 关闭策略。
    pub dismiss: OverlayDismiss,
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

/// 输入判定的系统阈值（默认值 = 本机 probe 实测；平台在 `platform_init` 覆盖为系统真值）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputThresholds {
    /// 双击的时间窗口（`GetDoubleClickTime()` = 500 ms）。
    pub double_click_time: Duration,
    /// 双击的位置容差（`SM_CXDOUBLECLK` = 4 px）。
    pub double_click_distance: f32,
    /// 「拖动 vs 点击」的位移阈值（`SM_CXDRAG` = 4 px）。
    pub drag_distance: f32,
}

impl Default for InputThresholds {
    fn default() -> Self {
        Self {
            double_click_time: Duration::from_millis(500),
            double_click_distance: 4.0,
            drag_distance: 4.0,
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

    /// **非追踪**读（handler 不是渲染 pass，不登记依赖）。
    pub fn read<T: Clone + 'static>(&self, signal: &crate::node::signal::Signal<T>) -> T {
        self.app.read(signal)
    }

    /// 写状态（`App::write` 的转调：等值短路 + `dependents` 反查 + 唤醒）。
    pub fn write<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        value: T,
    ) -> bool {
        self.app.write(signal, value)
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
/// 锚点对齐（`design/phase-10-interaction.md` §9.2）。
pub(crate) fn resolve_placement(
    tree: &crate::node::tree::NodeTree,
    placement: OverlayPlacement,
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
                AnchorSide::Above => Point::new(bounds.origin.x, bounds.origin.y - gap),
                AnchorSide::Left => Point::new(bounds.origin.x - gap, bounds.origin.y),
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

    #[test]
    fn thresholds_default_to_the_probed_values() {
        let thresholds = InputThresholds::default();

        assert_eq!(thresholds.double_click_time.as_millis(), 500);
        assert_eq!(thresholds.double_click_distance, 4.0);
        assert_eq!(thresholds.drag_distance, 4.0);
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

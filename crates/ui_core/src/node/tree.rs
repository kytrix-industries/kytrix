//! 持久节点树：`Node` / `NodeKind` / `LayoutBox` / `NodeTree`。
//!
//! 设计见 `design/phase-03-node-state.md` §11。要点：
//! - 唯一的一棵树（I5）：节点树 + arena（`NodeId` 带代际）+ DFS 先序数组 `order`；
//! - 子树在 `order` 里是**连续块**，因此结构变更只做局部拼接（`Vec::insert`/`drain` 的一段）；
//! - 所有写路径都走 `mark_*`（§9.3 的表），`touch` 顺带并 damage 并清 `REUSABLE`；
//! - `taffy::Cache` **不**内联（P04 在 `NodeTree` 旁建旁表，见 §11.1）。

mod arena;
mod content;
mod dirty;
mod layout;
mod order;
#[cfg(test)]
mod tests;
mod traverse;

use std::{fmt, rc::Rc};

use smallvec::SmallVec;
use smol_str::SmolStr;

use self::{content::TextSlot, layout::LayoutStats};
use crate::{
    error::EngineError,
    event::Action,
    interaction::{NodeInteraction, OverlaySlot, ScrollSlot, VirtualSlot},
    layout::{Axis, LayoutOverride, LayoutReport, MeasureFn, hit_test::HitResult},
    math::{Edges, PhysicalRect, Point, Rect, Size},
    node::{
        dependency::ReadCtx,
        dirty::DirtyFlags,
        id::{Arena, NodeId},
        signal::SignalId,
    },
    paint::{
        canvas::Canvas,
        geometry::GeometryId,
        scene::LaneId,
        style::{Clip, PaintSpec, Style, StylePatch},
    },
    text::{ShapedLine, TextContent},
};

/// 逻辑像素的布局结果（只记录 AABB 与边框）。
///
/// `bounds` 是**窗口绝对** border box AABB（P04 的绝对坐标 post-pass 写入）；`border` 来自 taffy
/// `Layout.border`，是 clip 的 padding box 与 P06 背景/边框绘制的来源。
/// `Copy`、32 B：damage 计算只需两次并集，不必借用。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutBox {
    /// 相对窗口左上角的逻辑像素 AABB（`w`/`h` 可为 0）。
    pub bounds: Rect,
    /// 四边 border 宽度（逻辑像素，CSS 顺序 上/右/下/左）。
    pub border: Edges,
}

/// 节点的种类（决定叶子性与绘制路径）。
///
/// 内部按平台无关的几何/画布形式存放（尚无平台窗口）。
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// 普通容器/叶子：由引擎按 `Style`/`StylePatch` 生成图元。
    Box,
    /// 自绘节点：挂载时提交的画布内容（P02 的 `Canvas`；`Rc` 共享，paint 只加引用计数）。
    Canvas(Rc<Canvas>),
    /// 静态几何节点：引用 P09 的 GPU 几何。
    Geometry(GeometryId),
    /// 文本叶子：载荷是 [`TextContent`]，布局由 `App::refresh_text` 注入的 measure 闭包完成。
    ///
    /// 在首次 `refresh_text` 之前它的 measure 为 `0×0`（`NodeBuilder::text` 不伪造闭包）。
    Text(TextContent),
}

impl NodeKind {
    /// 叶子节点（`Canvas`/`Geometry`/`Text`）不布局子节点 —— 消费者：`depends_on_children`、
    /// `NodeBuilder::build` 的 debug 断言。
    #[must_use]
    pub(crate) fn is_leaf(&self) -> bool {
        matches!(self, Self::Canvas(_) | Self::Geometry(_) | Self::Text(_))
    }

    /// 文本载荷（P06 的 paint 分派与 `App::refresh_text` 的唯一读入口）。
    #[must_use]
    pub(crate) fn text(&self) -> Option<&TextContent> {
        match self {
            Self::Text(content) => Some(content),
            _ => None,
        }
    }
}

/// 建节点所需的**部件**（`NodeBuilder` 收齐后一次性交给 [`NodeTree::insert_child`]）。
///
/// **字段全私有**：本 workspace 的 `clippy::field_scoped_visibility_modifiers = "forbid"` 禁止字段带
/// *scoped* 可见性修饰（`pub(crate)` 也不行，实测编译失败），因此只能走构造器 + 链式 `with_*`。
pub(crate) struct NodeSpec {
    kind: NodeKind,
    style: Style,
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    measure: Option<MeasureFn>,
    hit_shape: Option<Box<dyn Fn(Point) -> bool>>,
    layout_override: Option<LayoutOverride>,
    bind_text: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>,
    paint: Option<Box<PaintSpec>>,
    debug_id: Option<SmolStr>,
    action: Option<Action>,
    drag: Option<Action>,
    focus_ring: Option<crate::paint::style::FocusRing>,
    a11y: Option<crate::a11y::A11yOverride>,
    bind_a11y: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>>,
    /// 是否进入 Tab 序（窗口控件可以点击但不抢应用焦点）。
    tab_stop: bool,
}

/// 一个持久节点（跨帧存活）。**字段全私有**（I6：应用层不得持 `&mut Node`）。
pub struct Node {
    id: NodeId,
    parent: Option<NodeId>,
    children: SmallVec<[NodeId; 4]>,
    kind: NodeKind,
    /// 静态结构（挂载时一次写入，之后只有结构变更才重写）。
    style: Style,
    /// 动态属性补丁（`bind_style` 的**上次求值**结果）。
    style_patch: StylePatch,
    /// 动态属性闭包（`None` = 静态节点）。
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    /// 自定义叶子测量（P05 的文本入口；见 [`MeasureFn`] 的纯函数契约）。
    measure: Option<MeasureFn>,
    /// 自定义命中形状（局部坐标；`None` = 用 `layout.bounds` 判定）。
    hit_shape: Option<Box<dyn Fn(Point) -> bool>>,
    /// 布局覆盖（root 逃生口）。
    layout_override: Option<LayoutOverride>,
    /// 是否进入 Tab 序（`false` = 仍可点击 / 可被 AT 激活，但 `focus_next/prev` 跳过它）。
    tab_stop: bool,
    /// 静态绘制属性（P06；`None` = 不画背景/边框/阴影/渐变）。
    paint: Option<Box<PaintSpec>>,
    /// 内容盒相对 border box 左上角的偏移（`border + padding`；P06 的文本绝对坐标基准）。
    content_offset: Point,
    /// 动态文本闭包（`bind_text` 的求值入口；`None` = 静态文本）。
    text_bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>,
    /// 最近一次 shape 的内容尺寸（逻辑像素；`App::refresh_text` 判「尺寸变了没有」）。
    text_measured_size: Size,
    /// 最近一次布局的**内容宽**（逻辑像素；`App::layout_text` 的断行宽度，B1）。
    text_content_width: f32,
    /// 当前布局盒（逻辑像素；窗口绝对）。消费者：`Probe`/P06/P04 的命中测试。
    layout: LayoutBox,
    /// 上一次布局盒（damage 的「旧」一侧）。
    prev_layout: LayoutBox,
    /// taffy 写入的**父相对** border box（`set_unrounded_layout` 的唯一落点）。
    local: Rect,
    /// taffy 写入的四边 border 宽度。
    local_border: Edges,
    /// 最近一次被 taffy 布局的 epoch（`== NodeTree::layout_epoch` ⇒ 本轮布局过）。
    laid_out_epoch: u32,
    /// 本帧累积的逻辑 damage AABB（§9.4）。
    damage: Rect,
    /// 该节点的绘制输出桶（按绘制序；P06 的 paint 分配）。
    lanes: SmallVec<[LaneId; 2]>,
    /// 上次求值实际读过的信号（差分清理用；`replace_deps` 的 `old`）。
    deps: SmallVec<[SignalId; 8]>,
    dirty: DirtyFlags,
    /// 交互槽（点击/拖拽声明、滚动、虚拟列表、浮层）；`None` = 非交互节点。
    ///
    /// 一个 `Box` 装下全部字段（`Node` 只 +8 B，P10 预算 1136 ≤ 1152）。
    interaction: Option<Box<NodeInteraction>>,
    /// 该节点 lane 上次生成时的**累计滚动偏移**（paint 的复用判据，§7.6）。`Point::ZERO` = 未滚动。
    lane_offset: Point,
}

/// **手写 `Debug`**（N4）：`bind_style` / `measure` / `hit_shape` 是闭包（不实现 `Debug`），
/// 固定打印 `Some("<closure>")` / `None`；P04 新增的布局字段全部逐字段打印（否则测试失败信息与
/// P08 的 `Probe` 诊断会缺失布局事实）。
impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.id)
            .field("parent", &self.parent)
            .field("children", &self.children)
            .field("kind", &self.kind)
            .field("style", &self.style)
            .field("style_patch", &self.style_patch)
            .field("bind_style", &self.bind_style.as_ref().map(|_| "<closure>"))
            .field("measure", &self.measure.as_ref().map(|_| "<closure>"))
            .field("hit_shape", &self.hit_shape.as_ref().map(|_| "<closure>"))
            .field("layout_override", &self.layout_override)
            .field("paint", &self.paint)
            .field("content_offset", &self.content_offset)
            .field("text_bind", &self.text_bind.as_ref().map(|_| "<closure>"))
            .field("text_measured_size", &self.text_measured_size)
            .field("text_content_width", &self.text_content_width)
            .field("layout", &self.layout)
            .field("prev_layout", &self.prev_layout)
            .field("local", &self.local)
            .field("local_border", &self.local_border)
            .field("laid_out_epoch", &self.laid_out_epoch)
            .field("damage", &self.damage)
            .field("lanes", &self.lanes)
            .field("deps", &self.deps)
            .field("dirty", &self.dirty)
            .field("interaction", &self.interaction)
            .field("lane_offset", &self.lane_offset)
            .finish()
    }
}

impl Node {
    /// 节点的种类。
    #[must_use]
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    /// 静态样式。
    #[must_use]
    pub fn style(&self) -> &Style {
        &self.style
    }

    /// 是否进入 Tab 序（`NodeBuilder::tab_stop`；默认 `true`）。
    #[must_use]
    pub(crate) fn tab_stop(&self) -> bool {
        self.tab_stop
    }

    /// 上次求值的动态属性补丁（消费者：`App::refresh_styles`、`App::write` 的 `affects_layout`）。
    #[must_use]
    pub fn style_patch(&self) -> &StylePatch {
        &self.style_patch
    }

    /// 失效集合（消费者：`App::refresh_styles`、`StylePassReport`）。
    #[must_use]
    pub fn dirty(&self) -> DirtyFlags {
        self.dirty
    }

    /// 节点自己的句柄（消费者：P08 的 `Probe`、诊断日志）。
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// 父句柄；`None` = root。
    #[must_use]
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// 直接子节点（按插入序；消费者：P04 的布局遍历、P08 的 `Probe`）。
    #[must_use]
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    /// 本帧累积的 damage AABB（消费者：P06 的 damage 合并）。
    #[must_use]
    pub fn damage(&self) -> Rect {
        self.damage
    }

    /// 当前布局盒（P04 写入；`Probe` 读取）。
    #[must_use]
    pub fn layout(&self) -> LayoutBox {
        self.layout
    }

    /// 上一次布局盒（P04 写入；damage 的「旧」一侧）。
    #[must_use]
    pub fn prev_layout(&self) -> LayoutBox {
        self.prev_layout
    }

    /// 该节点占用的绘制桶（P06 的 paint 分配；`Probe` 读取）。
    #[must_use]
    pub fn lanes(&self) -> &[LaneId] {
        &self.lanes
    }

    /// 子树裁剪策略（P04 的命中测试与 P06 的 `Clip` 图元都读它）。
    #[must_use]
    pub fn clip(&self) -> Clip {
        self.style.clip
    }

    /// 自定义命中形状（局部坐标；`None` = 用 `layout.bounds` 判定）。
    #[must_use]
    pub fn hit_shape(&self) -> Option<&dyn Fn(Point) -> bool> {
        self.hit_shape.as_deref()
    }

    /// 布局覆盖（P10 的拖拽/分栏手柄；`None` = 由 taffy 决定）。
    #[must_use]
    pub fn layout_override(&self) -> Option<LayoutOverride> {
        self.layout_override
    }

    /// 静态绘制属性（消费者：`paint/pass.rs`）。
    #[must_use]
    pub(crate) fn paint_spec(&self) -> Option<&PaintSpec> {
        self.paint.as_deref()
    }

    /// 是否挂了动态闭包（`bind_style` / `bind_text`）。消费者：`Window::frame` 的 `debug_assert`
    /// （无 App 通道的窗口帧不跑样式/文本 pass，挂绑定就会读到过期补丁）。
    #[must_use]
    pub(crate) fn has_bindings(&self) -> bool {
        self.bind_style.is_some() || self.text_bind.is_some()
    }

    /// 把 `prev_layout ∪ layout` 并入 damage（§9.4：位置变化时新旧都要重画）。
    fn accumulate_damage(&mut self) {
        self.damage = self
            .damage
            .union(self.prev_layout.bounds)
            .union(self.layout.bounds);
    }
}

/// 唯一的一棵节点树（I5）。
///
/// `order` 是 DFS 先序数组：任何节点的子树在数组里占**连续**一段，因此结构变更只需拼接一段。
pub struct NodeTree {
    arena: Arena<Node>,
    root: NodeId,
    order: Vec<NodeId>,
    /// taffy 布局缓存旁表（按 arena 槽位下标索引；`Node` 不内联 `Cache`，见 P03 N6 / P04 §6.1）。
    layout_cache: Vec<Option<taffy::Cache>>,
    /// 文本形状快照旁表（按 arena 槽位下标；只有文本节点非空，16 B/槽）。
    text_slots: Vec<Option<TextSlot>>,
    /// 稳定调试 id 旁表（`NodeBuilder::id`；按 arena 槽位，槽位复用不继承）。
    debug_ids: Vec<Option<SmolStr>>,
    /// 每节点最近一次重建 lane 时的 clip 上下文（`(depth, covering)`；P06 §5.6 的 B1 守卫）。
    clip_keys: Vec<Option<(u8, Option<PhysicalRect>)>>,
    /// 每节点**子树包围盒**（绝对坐标；自身 + 全部后代的 layout.bounds 并集）。
    ///
    /// 命中测试的唯一安全子树剪枝依据（P2-2）。单看节点自己的布局盒不行：出流 / 溢出
    /// 子节点可以落在父盒之外（B2），按父盒剪枝会漏命中。这里是并集，只会偏大不会偏小。
    /// 每次 absolutize 末尾自底向上重算（O(N)，与布局同阶）。
    subtree_bounds: Vec<Rect>,
    /// a11y 载荷旁表（`NodeBuilder::{a11y,bind_a11y}`；按 arena 槽位，槽位复用不继承）。
    ///
    /// 与 `layout_cache` / `clip_keys` 同型：`Node` 因此保持 1144 B（P11 F-114）。
    a11y_slots: Vec<Option<Box<crate::a11y::tree::NodeA11y>>>,
    /// 带 `A11Y` 位的节点数（O(1) 的「有没有 a11y 工作」判据；P11 S-108）。
    a11y_dirty: u32,
    /// 文本内容可能变化的节点集合（P1-2.3）。
    ///
    /// `App::refresh_text` 原本每帧遍历整棵树、对每个文本节点 `text_content(node).cloned()`
    /// 再算全文哈希 —— 即使这一帧没有任何文本变化。集合在「文本载荷被写入」时登记
    /// （`set_text_content`）、在 `refresh_text` 消费后清空，因此稳态下遍历成本是 O(变化的文本)。
    ///
    /// 用 `Vec` + 线性去重而不是 `HashSet`：典型每帧 0–32 个条目，`Vec` 的扫描比哈希更快，
    /// 且迭代序确定（帧记录可复现）。
    text_dirty: Vec<NodeId>,
    /// 卸载节点的待释放 lane（`Window::paint_frame` 消费后清空，容量保留）。
    removed_lanes: Vec<LaneId>,
    /// 出流盒 → 它的包含块（taffy `add_hoisted_children` 的落点；唯一读者是 `absolutize`）。
    hoisted: ahash::HashMap<NodeId, SmallVec<[NodeId; 2]>>,
    /// 布局运行代次（0 保留，首轮为 1）。
    layout_epoch: u32,
    /// 本轮布局的计数器（`LayoutReport` 的来源）。
    layout_stats: LayoutStats,
    /// 首条 measure 违规（`InvalidMeasure` 的唯一来源）。
    invalid_measure: Option<EngineError>,
    /// 几何内容发生「隐藏 / 卸载 / 布局移动」时置位：这些路径拿不到 `GeometryDesc`，
    /// 无法精确算 `desc.bounds` 的溢出范围，只能保守地升级为全帧 damage（复审 A3）。
    /// `Window::paint_frame` 每帧取走。
    geometry_damage_escalation: bool,
}

impl Default for NodeTree {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeTree {
    // ── arena：实现见 `arena` 子模块的自由函数 ──
    /// 实现见子模块 `arena::new`（本文件只保留签名）。
    #[must_use]
    pub fn new() -> Self {
        arena::new()
    }
    /// 实现见子模块 `arena::root`（本文件只保留签名）。
    #[must_use]
    pub fn root(&self) -> NodeId {
        arena::root(self)
    }
    /// 实现见子模块 `arena::get`（本文件只保留签名）。
    pub fn get(&self, node: NodeId) -> Result<&Node, EngineError> {
        arena::get(self, node)
    }
    pub(crate) fn insert_child(
        &mut self,
        parent: NodeId,
        spec: NodeSpec,
    ) -> Result<NodeId, EngineError> {
        arena::insert_child(self, parent, spec)
    }
    /// 实现见子模块 `arena::remove`（本文件只保留签名）。
    pub fn remove(
        &mut self,
        node: NodeId,
    ) -> Result<Vec<(NodeId, SmallVec<[SignalId; 8]>)>, EngineError> {
        arena::remove(self, node)
    }
    /// 实现见子模块 `arena::debug_id`（本文件只保留签名）。
    #[must_use]
    pub fn debug_id(&self, node: NodeId) -> Option<&str> {
        arena::debug_id(self, node)
    }

    // ── order：实现见 `order` 子模块的自由函数 ──
    /// 实现见子模块 `order::order`（本文件只保留签名）。
    #[must_use]
    pub fn order(&self) -> &[NodeId] {
        order::order(self)
    }
    /// 实现见子模块 `order::append_child`（本文件只保留签名）。
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), EngineError> {
        order::append_child(self, parent, child)
    }
    fn order_insert(&mut self, parent: NodeId, child: NodeId) {
        order::order_insert(self, parent, child)
    }
    fn order_remove_subtree(&mut self, node: NodeId) {
        order::order_remove_subtree(self, node)
    }
    fn order_pos(&self, node: NodeId) -> Option<usize> {
        order::order_pos(self, node)
    }
    fn subtree_len(&self, node: NodeId) -> usize {
        order::subtree_len(self, node)
    }
    fn collect_subtree_ids(&self, node: NodeId) -> SmallVec<[NodeId; 16]> {
        order::collect_subtree_ids(self, node)
    }
    #[cfg(debug_assertions)]
    fn debug_assert_order_matches_tree(&self) {
        order::debug_assert_order_matches_tree(self)
    }

    // ── layout：实现见 `layout` 子模块的自由函数 ──
    pub(crate) fn compute_layout(
        &mut self,
        available: taffy::Size<Option<f32>>,
    ) -> Result<LayoutReport, EngineError> {
        layout::compute_layout(self, available)
    }
    pub(crate) fn invalidate_layout(&mut self, node: NodeId) -> bool {
        layout::invalidate_layout(self, node)
    }
    /// 实现见子模块 `layout::invalidate_measure`（本文件只保留签名）。
    pub fn invalidate_measure(&mut self, node: NodeId) -> Result<(), EngineError> {
        layout::invalidate_measure(self, node)
    }
    /// 实现见子模块 `layout::set_style`（本文件只保留签名）。
    pub fn set_style(&mut self, node: NodeId, style: Style) -> Result<(), EngineError> {
        layout::set_style(self, node, style)
    }
    /// 实现见子模块 `layout::set_layout_override`（本文件只保留签名）。
    pub fn set_layout_override(
        &mut self,
        node: NodeId,
        layout_override: Option<LayoutOverride>,
    ) -> Result<(), EngineError> {
        layout::set_layout_override(self, node, layout_override)
    }
    pub(crate) fn set_virtual_row(&mut self, node: NodeId, top: f32, height: f32) -> bool {
        layout::set_virtual_row(self, node, top, height)
    }
    pub(crate) fn set_absolute_inset(&mut self, node: NodeId, origin: Point) -> bool {
        layout::set_absolute_inset(self, node, origin)
    }
    pub(crate) fn taffy_style(&self, node: NodeId) -> &taffy::Style<SmolStr> {
        layout::taffy_style(self, node)
    }
    pub(crate) fn taffy_children(&self, node: NodeId) -> &[NodeId] {
        layout::taffy_children(self, node)
    }
    pub(crate) fn taffy_cache(&mut self, node: NodeId) -> &mut taffy::Cache {
        layout::taffy_cache(self, node)
    }
    pub(crate) fn set_local_layout(&mut self, node: NodeId, layout: &taffy::Layout) {
        layout::set_local_layout(self, node, layout)
    }
    pub(crate) fn measure_of(&self, node: NodeId) -> Option<&MeasureFn> {
        layout::measure_of(self, node)
    }
    pub(crate) fn layout_override_of(&self, node: NodeId) -> Option<LayoutOverride> {
        layout::layout_override_of(self, node)
    }
    pub(crate) fn is_out_of_flow(&self, node: NodeId) -> bool {
        layout::is_out_of_flow(self, node)
    }
    pub(crate) fn begin_layout(&mut self) {
        layout::begin_layout(self)
    }
    pub(crate) fn take_report(&mut self) -> LayoutReport {
        layout::take_report(self)
    }
    pub(crate) fn stats_mut(&mut self) -> &mut LayoutStats {
        layout::stats_mut(self)
    }
    pub(crate) fn note_invalid_measure(&mut self, node: NodeId, axis: Axis, value: f32) {
        layout::note_invalid_measure(self, node, axis, value)
    }
    pub(crate) fn take_invalid_measure(&mut self) -> Option<EngineError> {
        layout::take_invalid_measure(self)
    }
    pub(crate) fn absolutize(&mut self) {
        layout::absolutize(self)
    }
    fn place_subtree(
        &mut self,
        node: NodeId,
        reference: Point,
        epoch: u32,
        force: bool,
        parent_hidden: bool,
    ) {
        layout::place_subtree(self, node, reference, epoch, force, parent_hidden)
    }
    fn recompute_subtree_bounds(&mut self) {
        layout::recompute_subtree_bounds(self)
    }
    pub(crate) fn subtree_bounds(&self, node: NodeId) -> Rect {
        layout::subtree_bounds(self, node)
    }
    fn invalidate_caches_upwards(&mut self, node: NodeId) {
        layout::invalidate_caches_upwards(self, node)
    }
    pub(crate) fn settle_frame(&mut self) {
        layout::settle_frame(self)
    }
    fn bubble_layout(&mut self, from: NodeId) {
        layout::bubble_layout(self, from)
    }
    /// 实现见子模块 `layout::hit_test`（本文件只保留签名）。
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<HitResult> {
        layout::hit_test(self, point)
    }
    #[cfg(test)]
    pub(crate) fn set_layout_for_test(
        &mut self,
        node: NodeId,
        layout: LayoutBox,
        prev_layout: LayoutBox,
    ) -> bool {
        layout::set_layout_for_test(self, node, layout, prev_layout)
    }

    // ── dirty：实现见 `dirty` 子模块的自由函数 ──
    pub(crate) fn mark_paint(&mut self, node: NodeId) -> bool {
        dirty::mark_paint(self, node)
    }
    pub(crate) fn mark_layout(&mut self, node: NodeId) -> bool {
        dirty::mark_layout(self, node)
    }
    pub(crate) fn mark_a11y(&mut self, node: NodeId) -> bool {
        dirty::mark_a11y(self, node)
    }
    pub(crate) fn mark_structure(&mut self, node: NodeId) -> bool {
        dirty::mark_structure(self, node)
    }
    fn mark_subtree_paint(&mut self, node: NodeId) {
        dirty::mark_subtree_paint(self, node)
    }
    pub(crate) fn clear_paint(&mut self, node: NodeId) -> bool {
        dirty::clear_paint(self, node)
    }
    pub(crate) fn mark_moved_a11y(&mut self) -> usize {
        dirty::mark_moved_a11y(self)
    }
    pub(crate) fn clear_a11y(&mut self, node: NodeId) -> bool {
        dirty::clear_a11y(self, node)
    }
    pub(crate) fn a11y_dirty(&self) -> u32 {
        dirty::a11y_dirty(self)
    }
    fn touch(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        dirty::touch(self, node, flags)
    }
    fn touch_layout_only(&mut self, node: NodeId) -> bool {
        dirty::touch_layout_only(self, node)
    }
    pub(crate) fn any_node(&self, predicate: impl FnMut(&Node) -> bool) -> bool {
        dirty::any_node(self, predicate)
    }
    #[cfg(test)]
    pub(crate) fn clear_all_dirty_for_test(&mut self) {
        dirty::clear_all_dirty_for_test(self)
    }
    #[cfg(test)]
    pub(crate) fn set_dirty_for_test(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        dirty::set_dirty_for_test(self, node, flags)
    }

    // ── content：实现见 `content` 子模块的自由函数 ──
    pub(crate) fn text_content(&self, node: NodeId) -> Option<&TextContent> {
        content::text_content(self, node)
    }
    pub(crate) fn set_text_content(&mut self, node: NodeId, content: TextContent) -> bool {
        content::set_text_content(self, node, content)
    }
    fn mark_text_dirty(&mut self, node: NodeId) {
        content::mark_text_dirty(self, node)
    }
    pub(crate) fn take_text_dirty(&mut self, out: &mut Vec<NodeId>) {
        content::take_text_dirty(self, out)
    }
    #[cfg(test)]
    pub(crate) fn text_dirty_len(&self) -> usize {
        content::text_dirty_len(self)
    }
    pub(crate) fn set_text_shaped(
        &mut self,
        node: NodeId,
        shaped: Rc<crate::text::shape::ShapedText>,
        key: crate::text::cache::ShapeKey,
        scale_epoch: u32,
        measured: Size,
        size_changed: bool,
    ) -> bool {
        content::set_text_shaped(self, node, shaped, key, scale_epoch, measured, size_changed)
    }
    pub(crate) fn set_text_line(
        &mut self,
        node: NodeId,
        line: Rc<ShapedLine>,
        broken: parley::Layout<crate::paint::color::ColorBrush>,
        width: f32,
        generation: u32,
    ) -> bool {
        content::set_text_line(self, node, line, broken, width, generation)
    }
    pub(crate) fn text_content_width(&self, node: NodeId) -> f32 {
        content::text_content_width(self, node)
    }
    pub(crate) fn text_line(&self, node: NodeId) -> Option<&Rc<ShapedLine>> {
        content::text_line(self, node)
    }
    pub(crate) fn text_measured_size(&self, node: NodeId) -> Option<Size> {
        content::text_measured_size(self, node)
    }
    pub(crate) fn text_slot(&self, node: NodeId) -> Option<&TextSlot> {
        content::text_slot(self, node)
    }
    /// 实现见子模块 `content::text_selection_rects`（本文件只保留签名）。
    pub fn text_selection_rects(
        &self,
        node: NodeId,
        anchor: usize,
        focus: usize,
    ) -> Result<Vec<Rect>, EngineError> {
        content::text_selection_rects(self, node, anchor, focus)
    }
    /// 实现见子模块 `content::text_caret_rect`（本文件只保留签名）。
    pub fn text_caret_rect(&self, node: NodeId, byte_index: usize) -> Result<Rect, EngineError> {
        content::text_caret_rect(self, node, byte_index)
    }
    pub(crate) fn invalidate_text_layout(&mut self, node: NodeId) -> bool {
        content::invalidate_text_layout(self, node)
    }
    pub(crate) fn mark_all_text_dirty(&mut self) {
        content::mark_all_text_dirty(self)
    }
    pub(crate) fn mark_all_text_paint(&mut self) -> usize {
        content::mark_all_text_paint(self)
    }
    pub(crate) fn take_bind_text(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>> {
        content::take_bind_text(self, node)
    }
    pub(crate) fn put_bind_text(
        &mut self,
        node: NodeId,
        bind_text: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>,
    ) -> bool {
        content::put_bind_text(self, node, bind_text)
    }
    pub(crate) fn take_geometry_damage_escalation(&mut self) -> bool {
        content::take_geometry_damage_escalation(self)
    }
    pub(crate) fn content_offset(&self, node: NodeId) -> Point {
        content::content_offset(self, node)
    }
    pub(crate) fn lane_of(&self, node: NodeId) -> Option<LaneId> {
        content::lane_of(self, node)
    }
    pub(crate) fn set_lane(&mut self, node: NodeId, lane: LaneId) {
        content::set_lane(self, node, lane)
    }
    #[must_use]
    pub(crate) fn lane_offset(&self, node: NodeId) -> Point {
        content::lane_offset(self, node)
    }
    pub(crate) fn set_lane_offset(&mut self, node: NodeId, offset: Point) {
        content::set_lane_offset(self, node, offset)
    }
    pub(crate) fn clip_key(&self, node: NodeId) -> Option<(u8, Option<PhysicalRect>)> {
        content::clip_key(self, node)
    }
    pub(crate) fn set_clip_key(&mut self, node: NodeId, key: (u8, Option<PhysicalRect>)) {
        content::set_clip_key(self, node, key)
    }
    pub(crate) fn clear_clip_key(&mut self, node: NodeId) {
        content::clear_clip_key(self, node)
    }
    pub(crate) fn set_reusable(&mut self, node: NodeId) {
        content::set_reusable(self, node)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn clear_all_reusable(&mut self) {
        content::clear_all_reusable(self)
    }
    pub(crate) fn clear_lane(&mut self, node: NodeId) {
        content::clear_lane(self, node)
    }
    pub(crate) fn drain_removed_lanes(&mut self) -> std::vec::Drain<'_, LaneId> {
        content::drain_removed_lanes(self)
    }
    pub(crate) fn a11y_of(&self, node: NodeId) -> Option<&crate::a11y::tree::NodeA11y> {
        content::a11y_of(self, node)
    }
    pub(crate) fn take_bind_a11y(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>> {
        content::take_bind_a11y(self, node)
    }
    pub(crate) fn put_bind_a11y(
        &mut self,
        node: NodeId,
        bind: Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>,
    ) -> bool {
        content::put_bind_a11y(self, node, bind)
    }
    pub(crate) fn set_a11y_patch(&mut self, node: NodeId, patch: crate::a11y::A11yPatch) -> bool {
        content::set_a11y_patch(self, node, patch)
    }
    pub(crate) fn has_a11y_bind(&self, node: NodeId) -> bool {
        content::has_a11y_bind(self, node)
    }
    pub(crate) fn take_bind_style(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>> {
        content::take_bind_style(self, node)
    }
    pub(crate) fn put_bind_style(
        &mut self,
        node: NodeId,
        bind_style: Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>,
    ) -> bool {
        content::put_bind_style(self, node, bind_style)
    }
    pub(crate) fn deps(&self, node: NodeId) -> Option<&[SignalId]> {
        content::deps(self, node)
    }
    pub(crate) fn set_deps(&mut self, node: NodeId, deps: SmallVec<[SignalId; 8]>) -> bool {
        content::set_deps(self, node, deps)
    }
    pub(crate) fn set_style_patch(&mut self, node: NodeId, patch: StylePatch) -> bool {
        content::set_style_patch(self, node, patch)
    }
    pub(crate) fn has_visual_bind(&self, node: NodeId) -> bool {
        content::has_visual_bind(self, node)
    }
    #[must_use]
    pub(crate) fn interaction(&self, node: NodeId) -> Option<&NodeInteraction> {
        content::interaction(self, node)
    }
    pub(crate) fn ensure_interaction(&mut self, node: NodeId) -> Option<&mut NodeInteraction> {
        content::ensure_interaction(self, node)
    }
    #[must_use]
    pub(crate) fn is_hittable(&self, node: NodeId) -> bool {
        content::is_hittable(self, node)
    }
    #[must_use]
    pub(crate) fn focus_ring(&self, node: NodeId) -> Option<crate::paint::style::FocusRing> {
        content::focus_ring(self, node)
    }
    #[must_use]
    pub(crate) fn is_explicitly_interactive(&self, node: NodeId) -> bool {
        content::is_explicitly_interactive(self, node)
    }
    #[must_use]
    pub(crate) fn scroll_slot(&self, node: NodeId) -> Option<ScrollSlot> {
        content::scroll_slot(self, node)
    }
    pub(crate) fn scroll_slot_mut(&mut self, node: NodeId) -> Option<&mut ScrollSlot> {
        content::scroll_slot_mut(self, node)
    }
    #[must_use]
    pub(crate) fn overlay_slot(&self, node: NodeId) -> Option<OverlaySlot> {
        content::overlay_slot(self, node)
    }
    #[must_use]
    pub(crate) fn virtual_slot(&self, node: NodeId) -> Option<&VirtualSlot> {
        content::virtual_slot(self, node)
    }
    pub(crate) fn virtual_slot_mut(&mut self, node: NodeId) -> Option<&mut VirtualSlot> {
        content::virtual_slot_mut(self, node)
    }
    #[cfg(test)]
    pub(crate) fn set_style_for_test(&mut self, node: NodeId, style: Style) -> bool {
        content::set_style_for_test(self, node, style)
    }
    #[cfg(test)]
    pub(crate) fn set_style_patch_for_test(&mut self, node: NodeId, patch: StylePatch) -> bool {
        content::set_style_patch_for_test(self, node, patch)
    }
    #[cfg(test)]
    pub(crate) fn edit_style_for_test(&mut self, node: NodeId, edit: impl FnOnce(&mut Style)) {
        content::edit_style_for_test(self, node, edit)
    }

    // ── traverse：实现见 `traverse` 子模块的自由函数 ──
    #[must_use]
    pub(crate) fn is_visible_locally(&self, node: NodeId) -> bool {
        traverse::is_visible_locally(self, node)
    }
    #[must_use]
    pub(crate) fn is_effectively_visible(&self, node: NodeId) -> bool {
        traverse::is_effectively_visible(self, node)
    }
    fn is_ancestor(&self, ancestor: NodeId, node: NodeId) -> bool {
        traverse::is_ancestor(self, ancestor, node)
    }
    pub(crate) fn hoisted_clear(&mut self, node: NodeId) {
        traverse::hoisted_clear(self, node)
    }
    pub(crate) fn hoisted_push(&mut self, node: NodeId, hoisted: &[NodeId]) {
        traverse::hoisted_push(self, node, hoisted)
    }
}

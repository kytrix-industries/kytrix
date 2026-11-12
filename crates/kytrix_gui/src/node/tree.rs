//! 持久节点树：`Node` / `NodeKind` / `LayoutBox` / `NodeTree`。
//!
//! 设计见 `design/phase-03-node-state.md` §11。要点：
//! - 唯一的一棵树（I5）：节点树 + arena（`NodeId` 带代际）+ DFS 先序数组 `order`；
//! - 子树在 `order` 里是**连续块**，因此结构变更只做局部拼接（`Vec::insert`/`drain` 的一段）；
//! - 所有写路径都走 `mark_*`（§9.3 的表），`touch` 顺带并 damage 并清 `REUSABLE`；
//! - `taffy::Cache` **不**内联（P04 在 `NodeTree` 旁建旁表，见 §11.1）。

use std::{fmt, rc::Rc};

use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    error::EngineError,
    event::Action,
    interaction::{NodeInteraction, OverlaySlot, ScrollSlot, VirtualSlot},
    layout::{
        Axis, LayoutOverride, LayoutReport, MeasureFn,
        hit_test::{HitResult, hit_test},
    },
    math::{Edges, PhysicalRect, Point, Rect, Size},
    node::{
        dependency::ReadCtx,
        dirty::{DirtyFlags, affects_parent, depends_on_children},
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
    /// 自绘节点：挂载时提交的画布内容（P02 的 `Canvas`）。
    Canvas(Canvas),
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
}

impl NodeSpec {
    pub(crate) fn new(
        kind: NodeKind,
        style: Style,
        bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    ) -> Self {
        Self {
            kind,
            style,
            bind_style,
            measure: None,
            hit_shape: None,
            layout_override: None,
            bind_text: None,
            paint: None,
            debug_id: None,
            action: None,
            drag: None,
            focus_ring: None,
        }
    }

    pub(crate) fn with_focus_ring(mut self, ring: crate::paint::style::FocusRing) -> Self {
        self.focus_ring = Some(ring);
        self
    }

    pub(crate) fn with_action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    pub(crate) fn with_drag(mut self, action: Action) -> Self {
        self.drag = Some(action);
        self
    }

    pub(crate) fn with_paint(mut self, paint: PaintSpec) -> Self {
        self.paint = Some(Box::new(paint));
        self
    }

    pub(crate) fn with_debug_id(mut self, id: impl Into<SmolStr>) -> Self {
        self.debug_id = Some(id.into());
        self
    }

    pub(crate) fn with_measure(mut self, measure: MeasureFn) -> Self {
        self.measure = Some(measure);
        self
    }

    pub(crate) fn with_hit_shape(mut self, hit_shape: Box<dyn Fn(Point) -> bool>) -> Self {
        self.hit_shape = Some(hit_shape);
        self
    }

    pub(crate) fn with_layout_override(mut self, layout_override: LayoutOverride) -> Self {
        self.layout_override = Some(layout_override);
        self
    }

    pub(crate) fn with_bind_text(
        mut self,
        bind_text: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>,
    ) -> Self {
        self.bind_text = Some(bind_text);
        self
    }
}

impl Default for NodeSpec {
    fn default() -> Self {
        Self::new(NodeKind::Box, Style::default(), None)
    }
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

/// 一个文本节点的快照（P05）：`shaped` 供 measure/选区，`line` 供 P06 的 paint。
///
/// `key`/`scale_epoch`/`laid_*` 是「本帧是否需要重做」的判据（B4）：三者全等 ⇒ 整节点跳过。
pub(crate) struct TextSlot {
    shaped: Rc<crate::text::shape::ShapedText>,
    key: crate::text::cache::ShapeKey,
    scale_epoch: u32,
    line: Option<Rc<ShapedLine>>,
    /// 最终（按内容宽断行 + 对齐）的 parley 布局：选区映射的唯一真相（B1）。
    broken: Option<Box<parley::Layout<crate::paint::color::ColorBrush>>>,
    laid_width_bits: u32,
    laid_generation: u32,
}

impl TextSlot {
    pub(crate) fn shaped(&self) -> &Rc<crate::text::shape::ShapedText> {
        &self.shaped
    }

    pub(crate) fn key(&self) -> crate::text::cache::ShapeKey {
        self.key
    }

    pub(crate) fn scale_epoch(&self) -> u32 {
        self.scale_epoch
    }

    pub(crate) fn line(&self) -> Option<&Rc<ShapedLine>> {
        self.line.as_ref()
    }

    pub(crate) fn laid_width_bits(&self) -> u32 {
        self.laid_width_bits
    }

    pub(crate) fn laid_generation(&self) -> u32 {
        self.laid_generation
    }

    pub(crate) fn broken(&self) -> Option<&parley::Layout<crate::paint::color::ColorBrush>> {
        self.broken.as_deref()
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
}

/// 一次布局运行的计数器。
///
/// 字段私有 + `note_*`/`into_report` 方法：`field_scoped_visibility_modifiers` 禁止字段带 scoped 修饰，
/// 而可见性必须能被 `layout::taffy_tree` 使用，因此只能经方法暴露。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LayoutStats {
    cache_hits: usize,
    cache_misses: usize,
    laid_out: usize,
    moved: usize,
}

impl LayoutStats {
    pub(crate) fn note_cache_hit(&mut self) {
        self.cache_hits += 1;
    }

    pub(crate) fn note_cache_miss(&mut self) {
        self.cache_misses += 1;
    }

    /// 只有「完整布局且 taffy cache 未命中」的节点才计入 `laid_out`（= 真正被重算的节点）。
    pub(crate) fn note_laid_out(&mut self) {
        self.laid_out += 1;
    }

    pub(crate) fn into_report(self) -> LayoutReport {
        LayoutReport {
            laid_out: self.laid_out,
            moved: self.moved,
            cache_hits: self.cache_hits,
            cache_misses: self.cache_misses,
        }
    }
}

impl Default for NodeTree {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeTree {
    /// 建一棵只有根节点的树（消费者：`Window::new`）。
    ///
    /// **根默认撑满 ICB**（`size: 100% × 100%`）：taffy 的 `compute_root_layout` 只接受
    /// `available_space`，不会把「窗口尺寸」强加给根；flex/grid 根的 `size: auto` 是 shrink-to-fit
    /// （实测：窗口 200×200 时根会塌成 0×0，百分比子节点全部为 0）。因此根必须在建树时显式撑满。
    #[must_use]
    pub fn new() -> Self {
        let mut root_style = Style::default();
        root_style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::percent(1.0),
            height: taffy::style::Dimension::percent(1.0),
        };
        let mut arena: Arena<Node> = Arena::new();
        let root = arena.insert_with(|id| Node {
            id,
            parent: None,
            children: SmallVec::new(),
            kind: NodeKind::Box,
            style: root_style,
            style_patch: StylePatch::default(),
            bind_style: None,
            measure: None,
            hit_shape: None,
            layout_override: None,
            paint: None,
            content_offset: Point::ZERO,
            text_bind: None,
            text_measured_size: Size::ZERO,
            text_content_width: 0.0,
            layout: LayoutBox::default(),
            prev_layout: LayoutBox::default(),
            local: Rect::default(),
            local_border: Edges::ZERO,
            laid_out_epoch: 0,
            damage: Rect::default(),
            lanes: SmallVec::new(),
            deps: SmallVec::new(),
            dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
            interaction: None,
            lane_offset: Point::ZERO,
        });
        Self {
            arena,
            root,
            order: vec![root],
            layout_cache: vec![None],
            text_slots: vec![None],
            debug_ids: vec![None],
            clip_keys: vec![None],
            removed_lanes: Vec::new(),
            hoisted: ahash::HashMap::default(),
            layout_epoch: 0,
            layout_stats: LayoutStats::default(),
            invalid_measure: None,
        }
    }

    /// 根句柄（永不失效）。
    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// 取节点；陈旧句柄 → `EngineError::StaleNode`。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：节点从未存在或已卸载。
    pub fn get(&self, node: NodeId) -> Result<&Node, EngineError> {
        self.arena.get(node).ok_or(EngineError::StaleNode { node })
    }

    /// DFS 先序句柄数组（`root` 在首位）。消费者：`App::refresh_styles`、`App::close_window`。
    #[must_use]
    pub fn order(&self) -> &[NodeId] {
        &self.order
    }

    /// 把 `child`（连同子树）挂到 `parent` 的末尾；`child` 已有父则先摘下（局部拼接，不重建整树）。
    ///
    /// # Errors
    /// `EngineError::StaleNode`：`parent`/`child` 任一陈旧。
    ///
    /// # Panics (debug)
    /// 形成结构环（`child` 是 `parent` 的祖先或二者相同）：debug 断言失败，release 下**不改结构**。
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), EngineError> {
        self.get(parent)?;
        self.get(child)?;
        if parent == child || self.is_ancestor(child, parent) {
            debug_assert!(
                false,
                "structural cycle: cannot append an ancestor as a child"
            );
            return Ok(());
        }

        if let Some(old_parent) = self.get(child)?.parent {
            self.order_remove_subtree(child);
            if let Some(p) = self.arena.get_mut(old_parent) {
                p.children.retain(|existing| *existing != child);
            }
            self.mark_structure(old_parent);
            // 整棵被移动的子树都要重画：它的绘制桶顺序（`order`）在拼接后可能改变。
            self.mark_subtree_paint(child);
        }

        self.order_insert(parent, child);
        if let Some(node) = self.arena.get_mut(child) {
            node.parent = Some(parent);
        }
        if let Some(node) = self.arena.get_mut(parent) {
            node.children.push(child);
        }
        self.mark_structure(parent);
        #[cfg(debug_assertions)]
        self.debug_assert_order_matches_tree();
        Ok(())
    }

    /// 卸载 `node` 及其整棵子树（句柄全部变为陈旧）。
    ///
    /// # Errors
    /// `EngineError::StaleNode`：`node` 陈旧，或 `node` 是 root（root 不是可移除的子节点，见 N11）。
    pub fn remove(&mut self, node: NodeId) -> Result<(), EngineError> {
        if node == self.root {
            return Err(EngineError::StaleNode { node });
        }
        let parent = self
            .get(node)?
            .parent
            .expect("non-root node always has a parent");
        self.order_remove_subtree(node);
        if let Some(p) = self.arena.get_mut(parent) {
            p.children.retain(|existing| *existing != node);
        }
        let block = self.collect_subtree_ids(node);
        // 旧像素必须重绘：被卸载子树的包围盒并入父的 damage（出流/覆盖节点可能超出父盒，
        // 不能只靠 `mark_structure` 的父盒；P06 §5.5、不加 `LayoutBox.overflow` 的依据）。
        let mut subtree_damage = Rect::default();
        for id in &block {
            if let Some(target) = self.arena.get(*id) {
                subtree_damage = subtree_damage
                    .union(target.damage)
                    .union(target.layout.bounds);
                self.removed_lanes.extend(target.lanes.iter().copied());
            }
        }
        if let Some(p) = self.arena.get_mut(parent) {
            p.damage = p.damage.union(subtree_damage);
        }
        for id in block.iter().rev() {
            let index = id.index() as usize;
            if let Some(slot) = self.layout_cache.get_mut(index) {
                *slot = None;
            }
            if let Some(slot) = self.text_slots.get_mut(index) {
                *slot = None;
            }
            if let Some(slot) = self.debug_ids.get_mut(index) {
                *slot = None;
            }
            if let Some(slot) = self.clip_keys.get_mut(index) {
                *slot = None;
            }
            self.hoisted.remove(id);
            let _ = self.arena.remove(*id);
        }
        // 别的包含块可能把被卸载的节点记在自己的 hoisted 列表里（出流孙节点）⇒ 逐表剔除。
        self.hoisted.retain(|_, list| {
            list.retain(|child| !block.contains(child));
            !list.is_empty()
        });
        self.mark_structure(parent);
        #[cfg(debug_assertions)]
        self.debug_assert_order_matches_tree();
        Ok(())
    }

    /// 铸一个新节点并挂到 `parent` 末尾（消费者：`NodeBuilder::build`）。
    ///
    /// 参数是 [`NodeSpec`]（部件）而不是半成品 `Node`：`Node` 的全部字段都是本模块私有的，
    /// 只有这里能构造（P03 F-43 / P04 S-40）。
    pub(crate) fn insert_child(
        &mut self,
        parent: NodeId,
        spec: NodeSpec,
    ) -> Result<NodeId, EngineError> {
        self.get(parent)?;
        let NodeSpec {
            kind,
            style,
            bind_style,
            measure,
            hit_shape,
            layout_override,
            bind_text,
            paint,
            debug_id,
            action,
            drag,
            focus_ring,
        } = spec;
        let interaction = (action.is_some() || drag.is_some() || focus_ring.is_some()).then(|| {
            let mut slot = NodeInteraction::new();
            slot.action = action;
            slot.drag = drag;
            slot.focus_ring = focus_ring;
            Box::new(slot)
        });
        let id = self.arena.insert_with(move |id| Node {
            id,
            parent: None,
            children: SmallVec::new(),
            kind,
            style,
            style_patch: StylePatch::default(),
            bind_style,
            measure,
            hit_shape,
            layout_override,
            paint,
            content_offset: Point::ZERO,
            text_bind: bind_text,
            text_measured_size: Size::ZERO,
            text_content_width: 0.0,
            layout: LayoutBox::default(),
            prev_layout: LayoutBox::default(),
            local: Rect::default(),
            local_border: Edges::ZERO,
            laid_out_epoch: 0,
            damage: Rect::default(),
            lanes: SmallVec::new(),
            deps: SmallVec::new(),
            dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
            interaction,
            lane_offset: Point::ZERO,
        });
        // 槽位复用：上一代节点的 taffy 缓存必须丢弃，否则新节点继承陈旧布局（P04 §6.1）。
        let index = id.index() as usize;
        if self.layout_cache.len() <= index {
            self.layout_cache.resize_with(index + 1, || None);
        }
        self.layout_cache[index] = None;
        // N5：文本形状快照与 taffy 缓存同生命周期 —— 复用槽位时必须丢弃上一代的 `Rc<ShapedText>`。
        if self.text_slots.len() <= index {
            self.text_slots.resize_with(index + 1, || None);
        }
        self.text_slots[index] = None;
        // 稳定调试 id 与 clip 上下文同生命周期：槽位复用不得继承上一代的值（P06 §5.6 / C4）。
        if self.debug_ids.len() <= index {
            self.debug_ids.resize_with(index + 1, || None);
        }
        self.debug_ids[index] = debug_id;
        if self.clip_keys.len() <= index {
            self.clip_keys.resize_with(index + 1, || None);
        }
        self.clip_keys[index] = None;
        self.hoisted.remove(&id);
        self.order_insert(parent, id);
        if let Some(node) = self.arena.get_mut(id) {
            node.parent = Some(parent);
        }
        if let Some(node) = self.arena.get_mut(parent) {
            node.children.push(id);
        }
        self.mark_structure(parent);
        #[cfg(debug_assertions)]
        self.debug_assert_order_matches_tree();
        Ok(id)
    }

    /// 取走动态属性闭包（求值期；消费者：`App::refresh_styles`）。
    pub(crate) fn take_bind_style(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>> {
        self.arena.get_mut(node)?.bind_style.take()
    }

    /// 放回动态属性闭包（求值后；panic 展开时闭包丢失 → 该节点退化为静态节点）。
    pub(crate) fn put_bind_style(
        &mut self,
        node: NodeId,
        bind_style: Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>,
    ) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.bind_style = Some(bind_style);
                true
            }
            None => false,
        }
    }

    /// 上次求值的依赖（`replace_deps` 的 `old`；关窗退订）。
    pub(crate) fn deps(&self, node: NodeId) -> Option<&[SignalId]> {
        self.arena.get(node).map(|target| target.deps.as_slice())
    }

    /// 写回本次求值的依赖。
    pub(crate) fn set_deps(&mut self, node: NodeId, deps: SmallVec<[SignalId; 8]>) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.deps = deps;
                true
            }
            None => false,
        }
    }

    /// 写回动态属性补丁。
    pub(crate) fn set_style_patch(&mut self, node: NodeId, patch: StylePatch) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.style_patch = patch;
                true
            }
            None => false,
        }
    }

    /// 标 `PAINT`（不冒泡）。返回 `false` = 句柄陈旧。
    pub(crate) fn mark_paint(&mut self, node: NodeId) -> bool {
        self.touch(node, DirtyFlags::PAINT)
    }

    /// 标 `LAYOUT | PAINT` 并向上冒泡（§9.2 / N7），同时清 `node..root` 的 taffy 缓存（P04 §6.2）。
    pub(crate) fn mark_layout(&mut self, node: NodeId) -> bool {
        if !self.touch(node, DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
            return false;
        }
        self.bubble_layout(node);
        self.invalidate_caches_upwards(node);
        true
    }

    /// 标 `A11Y`（不冒泡）。
    pub(crate) fn mark_a11y(&mut self, node: NodeId) -> bool {
        self.touch(node, DirtyFlags::A11Y)
    }

    /// 结构变更：标 `LAYOUT|PAINT|ORDER|A11Y` 并向上冒泡，同时清 `node..root` 的 taffy 缓存。
    pub(crate) fn mark_structure(&mut self, node: NodeId) -> bool {
        if !self.touch(
            node,
            DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::ORDER | DirtyFlags::A11Y,
        ) {
            return false;
        }
        self.bubble_layout(node);
        self.invalidate_caches_upwards(node);
        true
    }

    /// 消费 `PAINT`（`refresh_styles` 求值完一个节点后调用）。
    pub(crate) fn clear_paint(&mut self, node: NodeId) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.dirty.remove(DirtyFlags::PAINT);
                true
            }
            None => false,
        }
    }

    // ── P06：lane / clip 上下文 / 帧结算 ──────────────────────────────────────

    /// 节点的绘制桶句柄（`P06` 每节点最多一条；消费者：`paint/pass.rs`）。
    pub(crate) fn lane_of(&self, node: NodeId) -> Option<LaneId> {
        self.arena
            .get(node)
            .and_then(|target| target.lanes.first().copied())
    }

    /// 记录节点的绘制桶（首个 lane；消费者：`paint/pass.rs`）。
    pub(crate) fn set_lane(&mut self, node: NodeId, lane: LaneId) {
        if let Some(target) = self.arena.get_mut(node) {
            if target.lanes.is_empty() {
                target.lanes.push(lane);
            } else {
                target.lanes[0] = lane;
            }
        }
    }

    /// 最近一次重建 lane 时的 clip 上下文 `(depth, covering)`（P06 §5.6 的 B1 守卫）。
    pub(crate) fn clip_key(&self, node: NodeId) -> Option<(u8, Option<PhysicalRect>)> {
        self.clip_keys.get(node.index() as usize).copied().flatten()
    }

    /// 写回本帧的 clip 上下文（消费者：`paint/pass.rs`）。
    pub(crate) fn set_clip_key(&mut self, node: NodeId, key: (u8, Option<PhysicalRect>)) {
        let index = node.index() as usize;
        if self.clip_keys.len() <= index {
            self.clip_keys.resize_with(index + 1, || None);
        }
        self.clip_keys[index] = Some(key);
    }

    /// 清空 clip 上下文（隐藏子树用：再次显示时必然失配 ⇒ 重建，避免复用空 lane）。
    pub(crate) fn clear_clip_key(&mut self, node: NodeId) {
        let index = node.index() as usize;
        if let Some(slot) = self.clip_keys.get_mut(index) {
            *slot = None;
        }
    }

    /// 标记节点可整桶复用（lane 内容与本帧的树/样式一致）。消费者：`paint/pass.rs`。
    pub(crate) fn set_reusable(&mut self, node: NodeId) {
        if let Some(target) = self.arena.get_mut(node) {
            target.dirty.insert(DirtyFlags::REUSABLE);
        }
    }

    /// 全树清 `REUSABLE`（消费者：`Window::force_full_repaint` 的 oracle 全量路径）。
    #[cfg(feature = "test-support")]
    pub(crate) fn clear_all_reusable(&mut self) {
        for id in &self.order {
            if let Some(target) = self.arena.get_mut(*id) {
                target.dirty.remove(DirtyFlags::REUSABLE);
            }
        }
    }

    /// 取走卸载节点的待释放 lane（消费者：`Window::paint_frame`；**借用式**清空，容量保留）。
    pub(crate) fn drain_removed_lanes(&mut self) -> std::vec::Drain<'_, LaneId> {
        self.removed_lanes.drain(..)
    }

    /// 丢弃节点的 lane 句柄（不放回 `Scene`；重建路径在"无图元"时先 `Scene::release_lane` 再调它）。
    pub(crate) fn clear_lane(&mut self, node: NodeId) {
        if let Some(target) = self.arena.get_mut(node) {
            target.lanes.clear();
        }
    }

    /// 稳定调试 id（`NodeBuilder::id` 的只读面；消费者：`Probe`、诊断日志）。
    ///
    /// 同一棵树内**允许重复**：`Selector::Id` 取 `order()` 的第一个（确定性），不做唯一性检查。
    #[must_use]
    pub fn debug_id(&self, node: NodeId) -> Option<&str> {
        self.debug_ids
            .get(node.index() as usize)
            .and_then(|slot| slot.as_ref())
            .map(SmolStr::as_str)
    }

    /// 内容盒相对 border box 左上角的偏移（消费者：`paint/pass.rs` 的文本绝对坐标）。
    pub(crate) fn content_offset(&self, node: NodeId) -> Point {
        self.arena
            .get(node)
            .map_or(Point::ZERO, |target| target.content_offset)
    }

    /// 帧结算（**唯一**的复位点，P06 §9.4 / F-59）：damage 归零、`prev_layout = layout`、
    /// 置 `REUSABLE`、清本帧已消费的 `LAYOUT | PAINT`。
    ///
    /// **只在提交时调用**（`Window::commit_frame`）：drain 阶段的 `touch` 发生在帧前，帧初归零会吞掉
    /// 已累积的 damage。帧内不得 `mark_*`（`&mut self` 已把写操作挡在帧外）。
    pub(crate) fn settle_frame(&mut self) {
        for id in &self.order {
            if let Some(target) = self.arena.get_mut(*id) {
                target.damage = Rect::default();
                target.prev_layout = target.layout;
                target.dirty.insert(DirtyFlags::REUSABLE);
                target.dirty.remove(DirtyFlags::LAYOUT | DirtyFlags::PAINT);
            }
        }
    }

    // ── P04：布局与命中的公开入口 ──────────────────────────────────────────────

    /// 结构性/静态样式变更的唯一公开入口：写值 + 标 `LAYOUT|PAINT` + 冒泡 + 清 `node..root` 的 taffy 缓存。
    ///
    /// **动态属性请用 `bind_style`**（`StylePatch` 通道）：每帧调本方法会清掉整条祖先链的缓存，
    /// 让增量布局退化为全量。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：句柄陈旧（从未存在 / 已卸载）。
    pub fn set_style(&mut self, node: NodeId, style: Style) -> Result<(), EngineError> {
        let Some(target) = self.arena.get_mut(node) else {
            return Err(EngineError::StaleNode { node });
        };
        target.style = style;
        self.invalidate_layout(node);
        Ok(())
    }

    /// 「我的 measure 结果变了，但样式没变」：唯一的 measure 失效入口（P05 的字体栈/文本内容变化）。
    ///
    /// 不做独立 measure 缓存：taffy `Cache` 的 key 已含 `(known_dimensions, available_space,
    /// sizing_mode, run_mode, parent_size)`，唯一缺的是「闭包捕获的状态变了」这个信号 —— 就是本方法。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：句柄陈旧。
    pub fn invalidate_measure(&mut self, node: NodeId) -> Result<(), EngineError> {
        if self.arena.get(node).is_none() {
            return Err(EngineError::StaleNode { node });
        }
        self.invalidate_layout(node);
        Ok(())
    }

    /// 更新布局覆盖矩形（P10 的拖拽/分栏手柄）：写值 + 标 `LAYOUT|PAINT` + 清 `node..root` 缓存。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：句柄陈旧。
    pub fn set_layout_override(
        &mut self,
        node: NodeId,
        layout_override: Option<LayoutOverride>,
    ) -> Result<(), EngineError> {
        let Some(target) = self.arena.get_mut(node) else {
            return Err(EngineError::StaleNode { node });
        };
        target.layout_override = layout_override;
        self.invalidate_layout(node);
        Ok(())
    }

    // ── P05：文本（载荷 / 形状快照 / 选区） ───────────────────────────────────

    /// 文本载荷（`None` = 不是文本节点）。消费者：`App::refresh_styles` / `App::refresh_text`。
    pub(crate) fn text_content(&self, node: NodeId) -> Option<&TextContent> {
        self.arena.get(node).and_then(|target| target.kind.text())
    }

    /// 写回文本载荷（`bind_text` 求值 + 静态 `text()` 的挂载路径）。
    pub(crate) fn set_text_content(&mut self, node: NodeId, content: TextContent) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => match &mut target.kind {
                NodeKind::Text(existing) => {
                    *existing = content;
                    true
                }
                _ => false,
            },
            None => false,
        }
    }

    /// 取走 `bind_text` 闭包（求值期间脱离节点，避免借用冲突）。
    pub(crate) fn take_bind_text(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>> {
        self.arena.get_mut(node)?.text_bind.take()
    }

    /// 放回 `bind_text` 闭包。
    pub(crate) fn put_bind_text(
        &mut self,
        node: NodeId,
        bind_text: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>,
    ) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.text_bind = Some(bind_text);
                true
            }
            None => false,
        }
    }

    /// 冻结形状快照并重建 measure 闭包。
    ///
    /// **无条件**清 `node..root` 的 taffy 缓存（B1）：taffy 的 cache 键不含「闭包捕获的状态」，
    /// snapshot 变化后旧 measure 结果必须作废；`size_changed` 只决定**是否额外**置 `LAYOUT`。
    pub(crate) fn set_text_shaped(
        &mut self,
        node: NodeId,
        shaped: Rc<crate::text::shape::ShapedText>,
        key: crate::text::cache::ShapeKey,
        scale_epoch: u32,
        measured: Size,
        size_changed: bool,
    ) -> bool {
        let Some(target) = self.arena.get_mut(node) else {
            return false;
        };
        let rc = Rc::clone(&shaped);
        target.measure = Some(Box::new(move |known, available| {
            rc.measure(known, available)
        }));
        target.text_measured_size = measured;

        let index = node.index() as usize;
        if self.text_slots.len() <= index {
            self.text_slots.resize_with(index + 1, || None);
        }
        match self.text_slots.get_mut(index).and_then(Option::as_mut) {
            Some(slot) => {
                slot.shaped = shaped;
                slot.key = key;
                slot.scale_epoch = scale_epoch;
                // 新快照 ⇒ 旧的断行/选区/布局依据全部作废（B4）：`layout_text` 的复用判据是
                // `line.is_some() && width_bits && generation`，任一不成立就重排。
                slot.line = None;
                slot.broken = None;
                slot.laid_width_bits = 0;
                slot.laid_generation = 0;
            }
            None => {
                if let Some(slot) = self.text_slots.get_mut(index) {
                    *slot = Some(TextSlot {
                        shaped,
                        key,
                        scale_epoch,
                        line: None,
                        broken: None,
                        laid_width_bits: 0,
                        laid_generation: 0,
                    });
                }
            }
        }

        self.invalidate_caches_upwards(node);
        if size_changed {
            self.mark_layout(node)
        } else {
            self.mark_paint(node)
        }
    }

    /// 挂上只读快照（P06 的 paint 输入）；`width`/`generation` 记下这次重排的依据。
    pub(crate) fn set_text_line(
        &mut self,
        node: NodeId,
        line: Rc<ShapedLine>,
        broken: parley::Layout<crate::paint::color::ColorBrush>,
        width: f32,
        generation: u32,
    ) -> bool {
        let index = node.index() as usize;
        let Some(slot) = self.text_slots.get_mut(index).and_then(Option::as_mut) else {
            return false;
        };
        slot.line = Some(line);
        slot.broken = Some(Box::new(broken));
        slot.laid_width_bits = width.to_bits();
        slot.laid_generation = generation;
        true
    }

    /// 最近一次布局的**内容宽**（逻辑像素；文本节点的断行宽度）。
    pub(crate) fn text_content_width(&self, node: NodeId) -> f32 {
        self.arena
            .get(node)
            .map_or(0.0, |target| target.text_content_width)
    }

    /// 只读快照（P06）。
    pub(crate) fn text_line(&self, node: NodeId) -> Option<&Rc<ShapedLine>> {
        self.text_slots
            .get(node.index() as usize)
            .and_then(Option::as_ref)
            .and_then(TextSlot::line)
    }

    /// 最近一次 shape 的内容尺寸（逻辑像素）。
    pub(crate) fn text_measured_size(&self, node: NodeId) -> Option<Size> {
        self.arena.get(node).map(|target| target.text_measured_size)
    }

    /// 文本快照（`shaped` 供 measure/选区）。
    pub(crate) fn text_slot(&self, node: NodeId) -> Option<&TextSlot> {
        self.text_slots
            .get(node.index() as usize)
            .and_then(Option::as_ref)
    }

    /// `(anchor, focus)` 字节偏移 → 文本盒局部的选区矩形（P10 的入口）。
    ///
    /// # Errors
    /// `StaleNode`：句柄陈旧；`TextShape`：不是文本节点或尚未 shape。
    pub fn text_selection_rects(
        &self,
        node: NodeId,
        anchor: usize,
        focus: usize,
    ) -> Result<Vec<Rect>, EngineError> {
        let Some(target) = self.arena.get(node) else {
            return Err(EngineError::StaleNode { node });
        };
        if target.kind.text().is_none() {
            return Err(EngineError::TextShape {
                node,
                detail: SmolStr::new_static("node is not a text node"),
            });
        }
        let Some(slot) = self.text_slot(node) else {
            return Err(EngineError::TextShape {
                node,
                detail: SmolStr::new_static("text has not been shaped yet"),
            });
        };
        let Some(broken) = slot.broken() else {
            return Err(EngineError::TextShape {
                node,
                detail: SmolStr::new_static("text has no final layout yet"),
            });
        };
        Ok(crate::text::inline_flow::selection_rects(
            broken,
            anchor,
            focus,
            slot.shaped().scale(),
        ))
    }

    /// 文本节点的布局失效（= `invalidate_measure` + 补 `PAINT`，F-56）。
    pub(crate) fn invalidate_text_layout(&mut self, node: NodeId) -> bool {
        if self.invalidate_measure(node).is_err() {
            return false;
        }
        self.mark_paint(node)
    }

    /// 一次布局运行：`compute_root_layout` → 绝对坐标 post-pass → 错误面。
    ///
    /// 消费者：`Window::compute_layout`（唯一调用点）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure { node, axis, value }`：measure 闭包返回非有限值或负尺寸（首条优先）。
    pub(crate) fn compute_layout(
        &mut self,
        available: taffy::Size<Option<f32>>,
    ) -> Result<LayoutReport, EngineError> {
        crate::layout::taffy_tree::run(self, available)
    }

    /// 命中测试（逆绘制序 + clip 链 + `hit_shape`）。
    ///
    /// 消费者：P10 的事件分发、P10 的 `Overlay`、P11 的 a11y hit test（P04 的用例也用它）。
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<HitResult> {
        hit_test(self, point)
    }

    // ── P10：交互槽的读写（消费者：`interaction/**`、`app.rs::dispatch_input`、`paint/pass.rs`） ──

    /// 交互槽（`None` = 非交互节点）。
    #[must_use]
    pub(crate) fn interaction(&self, node: NodeId) -> Option<&NodeInteraction> {
        self.arena
            .get(node)
            .and_then(|target| target.interaction.as_deref())
    }

    /// 确保交互槽存在（滚动 / 虚拟列表 / 浮层在挂载后挂槽的唯一入口）。
    pub(crate) fn ensure_interaction(&mut self, node: NodeId) -> Option<&mut NodeInteraction> {
        self.arena
            .get_mut(node)
            .map(|target| {
                target
                    .interaction
                    .get_or_insert_with(|| Box::new(NodeInteraction::new()))
            })
            .map(Box::as_mut)
    }

    /// 该节点是否可成为命中目标（`interactive == Some(false)` ⇒ 否；子节点照旧）。
    #[must_use]
    pub(crate) fn is_hittable(&self, node: NodeId) -> bool {
        self.arena
            .get(node)
            .is_some_and(|target| target.style_patch.interactive != Some(false))
    }

    /// 焦点环（`None` = 不画）。
    #[must_use]
    pub(crate) fn focus_ring(&self, node: NodeId) -> Option<crate::paint::style::FocusRing> {
        self.interaction(node).and_then(|slot| slot.focus_ring)
    }

    /// 该节点是否被显式标为可交互（`interactive == Some(true)`）。
    #[must_use]
    pub(crate) fn is_explicitly_interactive(&self, node: NodeId) -> bool {
        self.arena
            .get(node)
            .is_some_and(|target| target.style_patch.interactive == Some(true))
    }

    /// 滚动槽（`None` = 不是滚动容器）。
    #[must_use]
    pub(crate) fn scroll_slot(&self, node: NodeId) -> Option<ScrollSlot> {
        self.interaction(node).and_then(|slot| slot.scroll)
    }

    /// 滚动槽的可写引用。
    pub(crate) fn scroll_slot_mut(&mut self, node: NodeId) -> Option<&mut ScrollSlot> {
        self.ensure_interaction(node)
            .and_then(|slot| slot.scroll.as_mut())
    }

    /// 该节点 lane 上次生成时的累计滚动偏移（paint 的复用判据）。
    #[must_use]
    pub(crate) fn lane_offset(&self, node: NodeId) -> Point {
        self.arena
            .get(node)
            .map_or(Point::ZERO, |target| target.lane_offset)
    }

    /// 写 lane 的偏移戳（paint 重建 lane 时调用）。
    pub(crate) fn set_lane_offset(&mut self, node: NodeId, offset: Point) {
        if let Some(target) = self.arena.get_mut(node) {
            target.lane_offset = offset;
        }
    }

    /// 浮层槽（`None` = 不是浮层）。
    #[must_use]
    pub(crate) fn overlay_slot(&self, node: NodeId) -> Option<OverlaySlot> {
        self.interaction(node).and_then(|slot| slot.overlay)
    }

    /// 虚拟列表槽。
    #[must_use]
    pub(crate) fn virtual_slot(&self, node: NodeId) -> Option<&VirtualSlot> {
        self.interaction(node)
            .and_then(|slot| slot.virtual_list.as_ref())
    }

    /// 虚拟列表槽的可写引用。
    pub(crate) fn virtual_slot_mut(&mut self, node: NodeId) -> Option<&mut VirtualSlot> {
        self.ensure_interaction(node)
            .and_then(|slot| slot.virtual_list.as_mut())
    }

    /// 写一行虚拟列表的绝对定位（`left: 0 / right: 0 / top: i*extent / height: extent`）。
    ///
    /// 只碰 taffy 样式里「行位置」的四个字段（应用的行子树不受影响），随后走 `invalidate_layout`
    /// （`LAYOUT|PAINT` + 冒泡 + 清缓存）。
    pub(crate) fn set_virtual_row(&mut self, node: NodeId, top: f32, height: f32) -> bool {
        let Some(target) = self.arena.get_mut(node) else {
            return false;
        };
        target.style.inner.position = taffy::style::Position::Absolute;
        target.style.inner.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(0.0),
            right: taffy::style::LengthPercentageAuto::length(0.0),
            top: taffy::style::LengthPercentageAuto::length(top),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        target.style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::auto(),
            height: taffy::style::Dimension::length(height),
        };
        self.invalidate_layout(node)
    }

    /// 写绝对定位节点的左上角（浮层定位；只碰 `inset.left/top`）。
    pub(crate) fn set_absolute_inset(&mut self, node: NodeId, origin: Point) -> bool {
        let Some(target) = self.arena.get_mut(node) else {
            return false;
        };
        target.style.inner.position = taffy::style::Position::Absolute;
        target.style.inner.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(origin.x),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(origin.y),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        self.invalidate_layout(node)
    }

    // ── P04：taffy 低级 trait 的访问器（消费者：`layout::taffy_tree` / `layout::hit_test`） ──

    /// 节点的 taffy 样式；陈旧 id 回落到根的样式（`debug_assert`）。
    pub(crate) fn taffy_style(&self, node: NodeId) -> &taffy::Style<SmolStr> {
        match self.arena.get(node) {
            Some(target) => &target.style.inner,
            None => {
                debug_assert!(false, "taffy queried a stale node's style");
                &self.arena.get(self.root).expect("根节点恒存活").style.inner
            }
        }
    }

    /// 子节点切片；陈旧 id 回落为空切片。
    pub(crate) fn taffy_children(&self, node: NodeId) -> &[NodeId] {
        self.arena
            .get(node)
            .map_or(&[], |target| target.children.as_slice())
    }

    /// 旁表里的 taffy 缓存（首次访问即建）。
    pub(crate) fn taffy_cache(&mut self, node: NodeId) -> &mut taffy::Cache {
        let index = node.index() as usize;
        if self.layout_cache.len() <= index {
            self.layout_cache.resize_with(index + 1, || None);
        }
        self.layout_cache[index].get_or_insert_with(taffy::Cache::new)
    }

    /// `LayoutPartialTree::set_unrounded_layout` 的落点：写**父相对**几何 + 消费 `LAYOUT`。
    pub(crate) fn set_local_layout(&mut self, node: NodeId, layout: &taffy::Layout) {
        let epoch = self.layout_epoch;
        let local = Rect::new(
            layout.location.x,
            layout.location.y,
            layout.size.width,
            layout.size.height,
        );
        let border = Edges::new(
            layout.border.top,
            layout.border.right,
            layout.border.bottom,
            layout.border.left,
        );
        let content_width = (layout.size.width
            - layout.border.left
            - layout.border.right
            - layout.padding.left
            - layout.padding.right)
            .max(0.0);
        if let Some(target) = self.arena.get_mut(node) {
            target.local = local;
            target.local_border = border;
            target.text_content_width = content_width;
            target.content_offset = Point::new(
                layout.border.left + layout.padding.left,
                layout.border.top + layout.padding.top,
            );
            target.laid_out_epoch = epoch;
            // `LAYOUT` 位的唯一消费点：被 taffy 布局过 ⇒ 不再需要重排。
            target.dirty.remove(DirtyFlags::LAYOUT);
        }
    }

    /// 节点的自定义测量闭包。
    pub(crate) fn measure_of(&self, node: NodeId) -> Option<&MeasureFn> {
        self.arena
            .get(node)
            .and_then(|target| target.measure.as_ref())
    }

    /// 节点的布局覆盖。
    pub(crate) fn layout_override_of(&self, node: NodeId) -> Option<LayoutOverride> {
        self.arena
            .get(node)
            .and_then(|target| target.layout_override)
    }

    /// `LayoutContainingBlock::clear_hoisted_children`。
    pub(crate) fn hoisted_clear(&mut self, node: NodeId) {
        if let Some(list) = self.hoisted.get_mut(&node) {
            list.clear();
        }
    }

    /// `LayoutContainingBlock::add_hoisted_children`。
    pub(crate) fn hoisted_push(&mut self, node: NodeId, hoisted: &[NodeId]) {
        self.hoisted
            .entry(node)
            .or_default()
            .extend_from_slice(hoisted);
    }

    /// 本轮统计的可写视图（消费者：`impl CacheTree for NodeTree`、`dispatch_child_layout`）。
    pub(crate) fn stats_mut(&mut self) -> &mut LayoutStats {
        &mut self.layout_stats
    }

    /// 记录一条 measure 违规（**首条优先**，保证诊断确定）。
    pub(crate) fn note_invalid_measure(&mut self, node: NodeId, axis: Axis, value: f32) {
        if self.invalid_measure.is_none() {
            self.invalid_measure = Some(EngineError::InvalidMeasure { node, axis, value });
        }
    }

    pub(crate) fn take_invalid_measure(&mut self) -> Option<EngineError> {
        self.invalid_measure.take()
    }

    /// 开一轮布局：epoch +1、统计清零。
    pub(crate) fn begin_layout(&mut self) {
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
        self.layout_stats = LayoutStats::default();
    }

    /// 取走本轮统计（消费者：`layout::taffy_tree::run`）。
    pub(crate) fn take_report(&mut self) -> LayoutReport {
        std::mem::take(&mut self.layout_stats).into_report()
    }

    /// 标 `LAYOUT|PAINT` + 冒泡 + 清 `node..root` 的 taffy 缓存（所有「布局输入变了」的公共尾段）。
    ///
    /// 消费者：`set_style` / `invalidate_measure` / `set_layout_override` / `Window::{set_size, set_scale}`。
    pub(crate) fn invalidate_layout(&mut self, node: NodeId) -> bool {
        if !self.touch(node, DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
            return false;
        }
        self.bubble_layout(node);
        self.invalidate_caches_upwards(node);
        true
    }

    /// 绝对坐标 post-pass：把 `local`（父相对）换算成 `layout.bounds`（窗口绝对）。
    ///
    /// 遍历规则（正确性的核心，见 `design/phase-04-layout.md` §7）：
    /// - 从根开始，根的 `local` 相对 ICB 原点 ⇒ 绝对 = `local`；
    /// - 流内子节点：`abs = 父 abs.origin + child.local.origin`；
    /// - 出流子节点：**不在**父的 children 循环里处理（跳过），而由它**包含块**的 hoisted 列表驱动，
    ///   参照系仍是包含块的 border box origin（taffy `OofPositioningArea.offset` 的定义）；
    /// - 跳过整棵子树的条件：本轮没被布局（`laid_out_epoch != epoch`）且 `abs` 与旧值逐位相同。
    pub(crate) fn absolutize(&mut self) {
        let epoch = self.layout_epoch;
        self.place_subtree(self.root, Point::ZERO, epoch, true, false);
    }

    fn place_subtree(
        &mut self,
        node: NodeId,
        reference: Point,
        epoch: u32,
        force: bool,
        parent_hidden: bool,
    ) {
        let Some(target) = self.arena.get(node) else {
            return;
        };
        // 隐藏子树（自身或祖先 `display: none`）一律零化：taffy 的 hidden pass 已把 local 写成零，
        // 这里再兜住 `LayoutOverride`（覆盖节点不得留非零幽灵盒）。
        let hidden =
            parent_hidden || matches!(target.style.inner.display, taffy::style::Display::None);
        // `LayoutOverride` 直接覆盖 taffy 给的父相对盒（位置与尺寸都取应用给的值）；
        // 隐藏时归零（否则出流/隐藏的覆盖节点会在 P06 读到幽灵盒）。
        let local = if hidden {
            Rect::default()
        } else {
            target
                .layout_override
                .map_or(target.local, |over| over.rect)
        };
        let abs = Rect::from_origin_size(reference + local.origin, local.size);
        let old = target.layout;
        let children = target.children.clone();
        let changed = abs != old.bounds;
        let fresh = target.laid_out_epoch == epoch;
        if !force && !fresh && !changed {
            return;
        }

        if changed {
            self.layout_stats.moved += 1;
            if let Some(target) = self.arena.get_mut(node) {
                target.damage = target.damage.union(old.bounds).union(abs);
                target.prev_layout = old;
                target.layout = LayoutBox {
                    bounds: abs,
                    border: target.local_border,
                };
                // P06 F-60：lane 内的图元持有**绝对** bounds；布局移动后即使没有 `mark_paint`，
                // 旧 lane 也已过期（新位置的像素没人写）。
                target.dirty.remove(DirtyFlags::REUSABLE);
            }
        } else if fresh {
            // 尺寸/位置没变但 border 宽可能变了（样式改 border）。
            if let Some(target) = self.arena.get_mut(node) {
                target.layout.border = target.local_border;
            }
        }

        let hoisted: SmallVec<[NodeId; 2]> = self.hoisted.get(&node).cloned().unwrap_or_default();
        for child in children {
            // 出流节点（无 override）由它的**包含块**的 hoisted 列表驱动；但 override 节点按
            // **树父**的 border-box origin 放置（B1：taffy 会把 absolute 节点挂到最近的定位祖先，
            // 而引擎 `LayoutOverride` 的契约是「相对父 border box」），隐藏子树里的出流后代也必须
            // 在这里零化（N1）。
            if self.is_out_of_flow(child) && self.layout_override_of(child).is_none() && !hidden {
                continue;
            }
            self.place_subtree(child, abs.origin, epoch, false, hidden);
        }
        for child in hoisted {
            if self.layout_override_of(child).is_some() {
                // override 节点已由树父按 border-box origin 放置（B1），不得再用包含块的 local 算一次。
                continue;
            }
            self.place_subtree(child, abs.origin, epoch, false, false);
        }
    }

    /// 节点的布局是否「出流」（与 taffy `RoundTree::is_out_of_flow` 的判定逐字一致）。
    pub(crate) fn is_out_of_flow(&self, node: NodeId) -> bool {
        self.arena.get(node).is_some_and(|target| {
            target.style.inner.position.is_out_of_flow()
                && !matches!(target.style.inner.display, taffy::style::Display::None)
        })
    }

    /// 清 `node` 与**全部祖先**的 taffy 缓存（**不能早停**：任一祖先 cache 命中都会让 taffy 不再下钻）。
    fn invalidate_caches_upwards(&mut self, node: NodeId) {
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            let index = current.index() as usize;
            if let Some(cache) = self.layout_cache.get_mut(index).and_then(Option::as_mut) {
                cache.clear();
            }
            cursor = self.arena.get(current).and_then(|target| target.parent);
        }
    }

    /// 遍历全部存活节点并测试谓词（消费者：`StylePassReport` 的 `layout_dirty`/`paint_dirty`）。
    pub(crate) fn any_node(&self, mut predicate: impl FnMut(&Node) -> bool) -> bool {
        self.order
            .iter()
            .any(|id| self.arena.get(*id).is_some_and(&mut predicate))
    }

    /// 标记 + damage 并集 + 清 `REUSABLE`（唯一的写入口，§9.3）。
    fn touch(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        let Some(target) = self.arena.get_mut(node) else {
            return false;
        };
        target.dirty.insert(flags);
        if flags.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
            target.dirty.remove(DirtyFlags::REUSABLE);
            target.accumulate_damage();
        }
        true
    }

    /// `LAYOUT` 的向上冒泡（§9.2 的逐字伪代码）。
    fn bubble_layout(&mut self, from: NodeId) {
        let mut child = from;
        while let Some((parent, affects)) = self
            .arena
            .get(child)
            .map(|target| (target.parent, affects_parent(target)))
        {
            if !affects {
                break;
            }
            let Some(parent) = parent else {
                break;
            };
            let already = self
                .arena
                .get(parent)
                .is_some_and(|target| target.dirty.contains(DirtyFlags::LAYOUT));
            if already && self.arena.get(parent).is_some_and(depends_on_children) {
                break;
            }
            if !self.touch(parent, DirtyFlags::LAYOUT) {
                break;
            }
            if !self.arena.get(parent).is_some_and(depends_on_children) {
                break;
            }
            child = parent;
        }
    }

    /// 整棵子树标 `PAINT`（并清 `REUSABLE`）—— 重挂后绘制桶顺序可能变化（消费者：`append_child`）。
    fn mark_subtree_paint(&mut self, node: NodeId) {
        let ids = self.collect_subtree_ids(node);
        for id in ids {
            let _ = self.touch(id, DirtyFlags::PAINT);
        }
    }

    /// debug-only：`order` 必须是**全序**（DFS 先序、无重复、无遗漏、root 在首位）。
    #[cfg(debug_assertions)]
    fn debug_assert_order_matches_tree(&self) {
        let mut expected = SmallVec::<[NodeId; 32]>::new();
        let mut stack = SmallVec::<[NodeId; 32]>::new();
        stack.push(self.root);
        while let Some(current) = stack.pop() {
            expected.push(current);
            if let Some(target) = self.arena.get(current) {
                for child in target.children.iter().rev() {
                    stack.push(*child);
                }
            }
        }
        debug_assert_eq!(
            expected.as_slice(),
            self.order.as_slice(),
            "order 必须是 DFS 先序全序"
        );
    }

    /// `ancestor` 是否是 `node` 的（严格）祖先。
    fn is_ancestor(&self, ancestor: NodeId, node: NodeId) -> bool {
        let mut cursor = self.arena.get(node).and_then(|target| target.parent);
        while let Some(current) = cursor {
            if current == ancestor {
                return true;
            }
            cursor = self.arena.get(current).and_then(|target| target.parent);
        }
        false
    }

    /// `node` 在 `order` 里的位置（O(n) 线性扫描；P03 的最简版，见 V-28）。
    fn order_pos(&self, node: NodeId) -> Option<usize> {
        self.order.iter().position(|id| *id == node)
    }

    /// 子树节点数（迭代 DFS，不递归，深树也不爆栈）。
    fn subtree_len(&self, node: NodeId) -> usize {
        let mut count = 0;
        let mut stack = SmallVec::<[NodeId; 16]>::new();
        stack.push(node);
        while let Some(current) = stack.pop() {
            count += 1;
            if let Some(target) = self.arena.get(current) {
                stack.extend(target.children.iter().copied());
            }
        }
        count
    }

    /// 子树的 DFS 先序 id 序列。
    fn collect_subtree_ids(&self, node: NodeId) -> SmallVec<[NodeId; 16]> {
        let mut ids = SmallVec::new();
        let mut stack = SmallVec::<[NodeId; 16]>::new();
        stack.push(node);
        while let Some(current) = stack.pop() {
            ids.push(current);
            if let Some(target) = self.arena.get(current) {
                for child in target.children.iter().rev() {
                    stack.push(*child);
                }
            }
        }
        ids
    }

    /// 把 `child` 的子树块插到 `parent` 的子节点区的**末尾**（局部拼接）。
    fn order_insert(&mut self, parent: NodeId, child: NodeId) {
        let position = {
            let start = self
                .order_pos(parent)
                .map_or(self.order.len(), |pos| pos + 1);
            let mut position = start;
            if let Some(target) = self.arena.get(parent) {
                for existing in &target.children {
                    if *existing != child {
                        position += self.subtree_len(*existing);
                    }
                }
            }
            position
        };
        let block = self.collect_subtree_ids(child);
        for (offset, id) in block.iter().enumerate() {
            self.order.insert(position + offset, *id);
        }
    }

    /// 从 `order` 里删掉 `node` 的子树块（调用前该块必须是连续的）。
    fn order_remove_subtree(&mut self, node: NodeId) {
        let Some(position) = self.order_pos(node) else {
            return;
        };
        let len = self.subtree_len(node);
        self.order.drain(position..position + len);
    }

    /// 仅测试：清掉全部节点的失效位（让「标了什么」可被观察）。
    #[cfg(test)]
    pub(crate) fn clear_all_dirty_for_test(&mut self) {
        for id in &self.order {
            if let Some(target) = self.arena.get_mut(*id) {
                target.dirty = DirtyFlags::empty();
            }
        }
    }

    /// 仅测试：直接设置失效位。
    #[cfg(test)]
    pub(crate) fn set_dirty_for_test(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.dirty = flags;
                true
            }
            None => false,
        }
    }

    /// 仅测试：直接写布局盒（P04 的 `set_layout` 之前的唯一写点，damage 用例需要）。
    #[cfg(test)]
    pub(crate) fn set_layout_for_test(
        &mut self,
        node: NodeId,
        layout: LayoutBox,
        prev_layout: LayoutBox,
    ) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.layout = layout;
                target.prev_layout = prev_layout;
                true
            }
            None => false,
        }
    }

    /// 仅测试：直接替换静态样式。
    #[cfg(test)]
    pub(crate) fn set_style_for_test(&mut self, node: NodeId, style: Style) -> bool {
        match self.arena.get_mut(node) {
            Some(target) => {
                target.style = style;
                true
            }
            None => false,
        }
    }

    /// 仅测试：直接写样式补丁（paint pass 的 visible/opacity/background 用例需要）。
    #[cfg(test)]
    pub(crate) fn set_style_patch_for_test(&mut self, node: NodeId, patch: StylePatch) -> bool {
        self.set_style_patch(node, patch)
    }

    /// 仅测试：就地编辑静态样式。
    #[cfg(test)]
    pub(crate) fn edit_style_for_test(&mut self, node: NodeId, edit: impl FnOnce(&mut Style)) {
        if let Some(target) = self.arena.get_mut(node) {
            edit(&mut target.style);
        }
    }
}

#[cfg(test)]
mod tests {
    use smallvec::smallvec;

    use super::{LayoutBox, Node, NodeKind, NodeSpec, NodeTree};
    use crate::{
        error::EngineError,
        math::{Edges, Rect},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId, signal::SignalId},
        paint::style::{Clip, Style, StylePatch},
    };

    fn boxed(tree: &mut NodeTree, parent: NodeId) -> NodeId {
        NodeBuilder::boxed().build(tree, parent).expect("挂载成功")
    }

    #[test]
    fn new_tree_has_root() {
        let tree = NodeTree::new();
        let root = tree.root();

        assert_eq!(tree.order(), [root].as_slice());
        assert_eq!(tree.get(root).expect("root 存在").kind(), &NodeKind::Box);
        assert_eq!(
            tree.get(root).expect("root 存在").parent(),
            None,
            "root 无父"
        );
    }

    #[test]
    fn append_child_keeps_dfs_preorder() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);
        let b = boxed(&mut tree, root);

        assert_eq!(tree.order(), [root, a, a1, b].as_slice());
        assert_eq!(tree.get(a).expect("存在").parent(), Some(root));
        assert_eq!(tree.get(a1).expect("存在").parent(), Some(a));
    }

    #[test]
    fn append_child_reparents_and_splices() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);
        let b = boxed(&mut tree, root);
        let b1 = boxed(&mut tree, b);

        tree.append_child(a, b).expect("重挂成功");

        assert_eq!(
            tree.order(),
            [root, a, a1, b, b1].as_slice(),
            "子树块必须整体搬到新父的末尾，且不重复"
        );
        assert_eq!(tree.get(b).expect("存在").parent(), Some(a));
        assert_eq!(tree.get(b1).expect("存在").parent(), Some(b));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "structural cycle")]
    fn append_child_rejects_cycle() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);

        // 把祖先挂成后代的子节点：结构环。
        let _ = tree.append_child(a1, a);
    }

    #[test]
    fn remove_unmounts_subtree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = boxed(&mut tree, root);
        let a1 = boxed(&mut tree, a);
        let a2 = boxed(&mut tree, a);
        let b = boxed(&mut tree, root);

        tree.remove(a).expect("卸载成功");

        assert_eq!(tree.order(), [root, b].as_slice());
        for stale in [a, a1, a2] {
            assert!(
                matches!(tree.get(stale), Err(EngineError::StaleNode { .. })),
                "子树内句柄必须全部陈旧"
            );
        }
        assert!(tree.get(b).is_ok());
    }

    #[test]
    fn remove_root_is_error() {
        let mut tree = NodeTree::new();
        let root = tree.root();

        assert!(matches!(
            tree.remove(root),
            Err(EngineError::StaleNode { node }) if node == root
        ));
        assert_eq!(tree.order(), [root].as_slice(), "root 必须还在");
    }

    #[test]
    fn append_child_stale_ids_are_errors() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let doomed = boxed(&mut tree, root);
        tree.remove(doomed).expect("卸载成功");

        assert!(matches!(
            tree.append_child(doomed, root),
            Err(EngineError::StaleNode { node }) if node == doomed
        ));
        assert!(matches!(
            tree.append_child(root, doomed),
            Err(EngineError::StaleNode { node }) if node == doomed
        ));
    }

    #[test]
    fn insert_child_and_bind_style_roundtrip() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = tree
            .insert_child(
                root,
                NodeSpec::new(
                    NodeKind::Box,
                    Style::default(),
                    Some(Box::new(|_cx| StylePatch::default())),
                ),
            )
            .expect("插入成功");

        assert!(tree.take_bind_style(node).is_some(), "闭包必须被取走");
        assert!(tree.take_bind_style(node).is_none(), "取走后为 None");
        assert!(tree.put_bind_style(node, Box::new(|_cx| StylePatch::default())));
        assert!(tree.take_bind_style(node).is_some());

        assert_eq!(tree.deps(node), Some([].as_slice()));
        assert!(tree.set_deps(node, smallvec![SignalId::for_test(0)]));
        assert_eq!(tree.deps(node).map(<[SignalId]>::len), Some(1));
        assert!(tree.set_style_patch(node, StylePatch::default()));
    }

    #[test]
    fn node_and_handle_sizes() {
        let node_size = size_of::<Node>();
        println!("size_of::<Node>() = {node_size}");
        println!("size_of::<LayoutBox>() = {}", size_of::<LayoutBox>());
        println!("size_of::<NodeKind>() = {}", size_of::<NodeKind>());
        println!(
            "size_of::<TextContent>() = {}",
            size_of::<crate::text::TextContent>()
        );
        assert!(
            node_size <= 1152,
            "Node 尺寸超预算: {node_size} B（过 1152 → 备选 style: Box<Style>，见 V-31）"
        );
        assert_eq!(
            size_of::<crate::text::TextContent>(),
            112,
            "TextContent 的尺寸是 P05 的预算基线（S-53：含 inline_boxes）"
        );
        assert_eq!(size_of::<Option<NodeId>>(), 8, "Option<NodeId> 必须零开销");
        assert_eq!(size_of::<NodeId>(), 8);
    }

    #[test]
    fn layout_box_is_32_bytes() {
        assert_eq!(
            size_of::<LayoutBox>(),
            size_of::<Rect>() + size_of::<Edges>()
        );
        assert_eq!(size_of::<LayoutBox>(), 32);
    }

    #[test]
    fn root_style_is_percent_100() {
        use taffy::style::{Dimension, ExpandedDimension};

        let tree = NodeTree::new();
        let root = tree.get(tree.root()).expect("root 存在");
        let size = root.style().inner.size;

        assert!(matches!(size.width.expand(), ExpandedDimension::Percent(v) if v == 1.0));
        assert!(matches!(size.height.expand(), ExpandedDimension::Percent(v) if v == 1.0));
        assert_eq!(size.width, Dimension::percent(1.0));
    }

    #[test]
    fn slot_reuse_drops_cache() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let doomed = boxed(&mut tree, root);
        let index = doomed.index();
        // 建一个缓存条目
        let _ = tree.taffy_cache(doomed);
        assert!(
            tree.layout_cache[index as usize].is_some(),
            "缓存条目必须建立"
        );

        tree.remove(doomed).expect("卸载成功");
        assert!(
            tree.layout_cache[index as usize].is_none(),
            "卸载必须清掉槽位的缓存"
        );

        let reused = boxed(&mut tree, root);
        assert_eq!(reused.index(), index, "槽位必须复用");
        assert!(
            tree.layout_cache[index as usize].is_none(),
            "复用槽位不得继承上一代节点的缓存"
        );
    }

    #[test]
    fn set_style_clears_caches_to_root() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = boxed(&mut tree, root);
        let child = boxed(&mut tree, parent);
        let sibling = boxed(&mut tree, root);
        let _ = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(100.0),
                height: Some(100.0),
            })
            .expect("布局成功");
        let sibling_index = sibling.index() as usize;
        assert!(
            tree.layout_cache[sibling_index].is_some(),
            "布局后兄弟必须有缓存条目"
        );

        tree.set_style(child, Style::default()).expect("节点存活");

        for node in [root, parent, child] {
            let index = node.index() as usize;
            let empty = tree.layout_cache[index]
                .as_ref()
                .is_none_or(taffy::Cache::is_empty);
            assert!(empty, "node {node:?} 的缓存必须被清空");
        }
        assert!(
            tree.layout_cache[sibling_index]
                .as_ref()
                .is_some_and(|cache| !cache.is_empty()),
            "无关兄弟的缓存必须保留"
        );
    }

    #[test]
    fn remove_drops_cache_and_hoisted_entries() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = boxed(&mut tree, root);
        let child = boxed(&mut tree, parent);
        let child_index = child.index() as usize;
        let _ = tree.taffy_cache(child);
        tree.hoisted_push(parent, &[child]);

        tree.remove(parent).expect("卸载成功");

        assert!(
            tree.layout_cache[child_index].is_none(),
            "子树内每个槽位的缓存都要清"
        );
        assert!(
            !tree.hoisted.contains_key(&parent),
            "包含块的 hoisted 列表要清"
        );
        assert!(!tree.hoisted.contains_key(&child));

        // N5：别的包含块若把被卸载节点记在自己的列表里，也必须被逐表剔除
        let other = boxed(&mut tree, root);
        let victim = boxed(&mut tree, root);
        tree.hoisted_push(other, &[victim]);
        tree.remove(victim).expect("卸载被记录的节点");
        assert!(
            tree.hoisted
                .get(&other)
                .is_none_or(|list| !list.contains(&victim)),
            "其它包含块的列表不得残留被卸载 id"
        );
    }

    #[test]
    fn node_clip_reads_style() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let style = Style {
            clip: Clip::Bounds,
            ..Style::default()
        };
        let node = tree
            .insert_child(root, NodeSpec::new(NodeKind::Box, style, None))
            .expect("插入成功");

        assert_eq!(tree.get(node).expect("存在").clip(), Clip::Bounds);
    }

    #[test]
    fn node_debug_prints_layout_fields() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = boxed(&mut tree, root);

        let debug = format!("{:?}", tree.get(node).expect("存在"));

        for field in [
            "layout_override",
            "local",
            "local_border",
            "laid_out_epoch",
            "measure",
            "hit_shape",
        ] {
            assert!(debug.contains(field), "Debug 缺字段 {field}: {debug}");
        }
    }

    #[test]
    fn damage_unions_prev_and_current() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = boxed(&mut tree, root);
        tree.set_layout_for_test(
            node,
            LayoutBox {
                border: Edges::ZERO,
                bounds: Rect::new(10.0, 0.0, 4.0, 4.0),
            },
            LayoutBox {
                border: Edges::ZERO,
                bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            },
        );
        assert!(
            tree.get(node).expect("存在").damage().is_empty(),
            "布局写入本身不产生 damage（damage 由 touch 累积）"
        );

        assert!(tree.mark_paint(node));

        let damage = tree.get(node).expect("存在").damage();
        assert_eq!(
            damage,
            Rect::new(0.0, 0.0, 14.0, 4.0),
            "旧位置与新位置的并集必须都覆盖"
        );
    }

    #[test]
    fn remove_marks_parent_structure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = boxed(&mut tree, root);
        let child = boxed(&mut tree, parent);
        let _grandchild = boxed(&mut tree, child);
        tree.clear_all_dirty_for_test();

        tree.remove(child).expect("卸载成功");

        let parent_flags = tree.get(parent).expect("存在").dirty();
        for expected in [
            DirtyFlags::LAYOUT,
            DirtyFlags::PAINT,
            DirtyFlags::ORDER,
            DirtyFlags::A11Y,
        ] {
            assert!(
                parent_flags.contains(expected),
                "结构变更必须标 {expected:?}（实际 {parent_flags:?}）"
            );
        }
        assert!(
            tree.get(root)
                .expect("存在")
                .dirty()
                .contains(DirtyFlags::LAYOUT),
            "父是 auto 链 → 必须冒泡到根"
        );
        assert!(
            tree.get(parent).expect("存在").children().is_empty(),
            "被移除的子树必须从父的 children 里摘掉"
        );
    }

    #[test]
    fn append_child_marks_moved_subtree_paint() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let from = boxed(&mut tree, root);
        let to = boxed(&mut tree, root);
        let child = boxed(&mut tree, from);
        let grandchild = boxed(&mut tree, child);
        tree.set_dirty_for_test(child, DirtyFlags::REUSABLE);
        tree.set_dirty_for_test(grandchild, DirtyFlags::REUSABLE | DirtyFlags::LAYOUT);

        tree.append_child(to, child).expect("重挂成功");

        for moved in [child, grandchild] {
            let flags = tree.get(moved).expect("存在").dirty();
            assert!(
                flags.contains(DirtyFlags::PAINT),
                "被移动子树的每个节点都必须标 PAINT（实际 {flags:?}）"
            );
            assert!(
                !flags.contains(DirtyFlags::REUSABLE),
                "REUSABLE 必须被清（实际 {flags:?}）"
            );
        }
        assert!(
            tree.get(to)
                .expect("存在")
                .dirty()
                .contains(DirtyFlags::ORDER)
        );
    }

    // ── P05：文本节点 ────────────────────────────────────────────────────────

    fn text_tree(text: &str) -> (NodeTree, NodeId) {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::text(text, crate::text::TextStyle::new(16.0))
            .build(&mut tree, root)
            .expect("挂载成功");
        (tree, node)
    }

    #[test]
    fn text_kind_is_a_leaf_and_exposes_its_content() {
        let (tree, node) = text_tree("hello");

        assert!(tree.get(node).expect("存在").kind().is_leaf());
        let content = tree.text_content(node).expect("文本载荷");
        assert_eq!(content.text, "hello");
        assert!(tree.text_line(node).is_none(), "尚未 shape");
        assert!(tree.text_slot(node).is_none());
    }

    #[test]
    fn remove_drops_the_text_slot() {
        let (mut tree, node) = text_tree("temp");
        assert!(tree.remove(node).is_ok());

        assert!(tree.text_slot(node).is_none(), "槽位必须随卸载清空（N5）");
        assert!(tree.text_line(node).is_none());
    }

    #[test]
    fn text_selection_rects_reports_preconditions() {
        let (tree, node) = text_tree("abcdef");

        let error = tree
            .text_selection_rects(node, 0, 3)
            .expect_err("尚未 shape 的节点必须报错");
        assert!(matches!(
            error,
            EngineError::TextShape { node: reported, .. } if reported == node
        ));

        let root = tree.root();
        let error = tree
            .text_selection_rects(root, 0, 3)
            .expect_err("非文本节点必须报错");
        assert!(matches!(error, EngineError::TextShape { .. }));
    }

    #[test]
    fn text_content_updates_only_text_nodes() {
        let (mut tree, node) = text_tree("one");
        let root = tree.root();

        assert!(tree.set_text_content(
            node,
            crate::text::TextContent::new("two", crate::text::TextStyle::new(12.0))
        ));
        assert_eq!(tree.text_content(node).expect("文本").text, "two");
        assert!(
            !tree.set_text_content(
                root,
                crate::text::TextContent::new("nope", crate::text::TextStyle::default())
            ),
            "非文本节点必须拒绝写载荷"
        );
    }

    #[test]
    fn text_bind_text_round_trips() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::text("x", crate::text::TextStyle::new(16.0))
            .bind_text(|_cx| smol_str::SmolStr::new_static("bound"))
            .build(&mut tree, root)
            .expect("挂载成功");

        let closure = tree.take_bind_text(node).expect("闭包必须存在");
        assert!(tree.take_bind_text(node).is_none(), "取走后为空");
        assert!(tree.put_bind_text(node, closure));
        assert!(tree.take_bind_text(node).is_some(), "放回后仍可取");
    }

    // ── P06：settle / lane / debug id / clip key ──

    fn layout_box(bounds: Rect) -> LayoutBox {
        LayoutBox {
            bounds,
            border: Edges::ZERO,
        }
    }

    #[test]
    fn settle_frame_resets_damage_bits_and_prev_layout() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let child = boxed(&mut tree, root);
        let _ = tree.set_layout_for_test(
            child,
            layout_box(Rect::new(1.0, 2.0, 3.0, 4.0)),
            LayoutBox::default(),
        );
        let _ = tree.mark_paint(child);
        let _ = tree.mark_a11y(child);
        let flags = tree.get(child).expect("存在").dirty();
        assert!(flags.contains(DirtyFlags::PAINT));
        assert!(!tree.get(child).expect("存在").damage().is_empty());

        tree.settle_frame();

        let node = tree.get(child).expect("存在");
        assert!(node.damage().is_empty(), "damage 归零");
        assert_eq!(node.prev_layout(), node.layout(), "prev_layout = layout");
        assert!(node.dirty().contains(DirtyFlags::REUSABLE));
        assert!(!node.dirty().contains(DirtyFlags::PAINT));
        assert!(!node.dirty().contains(DirtyFlags::LAYOUT));
        assert!(
            node.dirty().contains(DirtyFlags::A11Y),
            "A11Y 的消费者不在本帧（P11），结算必须保留"
        );
    }

    #[test]
    fn remove_unions_subtree_bounds_into_parent_damage() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = boxed(&mut tree, root);
        let child = boxed(&mut tree, parent);
        // 出流子节点在父盒之外。
        let _ = tree.set_layout_for_test(
            parent,
            layout_box(Rect::new(0.0, 0.0, 10.0, 10.0)),
            LayoutBox::default(),
        );
        let outside = Rect::new(100.0, 100.0, 20.0, 20.0);
        let _ = tree.set_layout_for_test(child, layout_box(outside), LayoutBox::default());
        tree.settle_frame();

        tree.remove(child).expect("卸载成功");

        let damage = tree.get(parent).expect("存在").damage();
        assert!(
            damage.union(outside) == damage,
            "父 damage 必须覆盖被卸载子树的旧盒：{damage:?}"
        );
    }

    #[test]
    fn remove_reports_released_lanes() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = boxed(&mut tree, root);
        let child = boxed(&mut tree, parent);
        let lane = crate::paint::scene::LaneId::for_test(0, 1);
        tree.set_lane(parent, crate::paint::scene::LaneId::for_test(1, 1));
        tree.set_lane(child, lane);

        tree.remove(parent).expect("卸载成功");

        let removed: Vec<_> = tree.drain_removed_lanes().collect();
        assert_eq!(removed.len(), 2, "父 + 子各一条 lane");
        assert!(removed.contains(&lane));
        assert_eq!(tree.drain_removed_lanes().count(), 0, "取走即清空");
    }

    #[test]
    fn debug_id_roundtrip_and_slot_reuse() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .id("label")
            .build(&mut tree, root)
            .expect("挂载成功");
        assert_eq!(tree.debug_id(node), Some("label"));

        tree.remove(node).expect("卸载");
        let replacement = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("挂载成功");
        assert_eq!(replacement.index(), node.index(), "槽位应复用");
        assert_eq!(tree.debug_id(replacement), None, "新节点不得继承 debug id");
    }

    #[test]
    fn clip_key_roundtrip_and_clear() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let child = boxed(&mut tree, root);
        assert_eq!(tree.clip_key(child), None);
        let covering = Rect::new(0.0, 0.0, 10.0, 10.0)
            .to_physical_covering(crate::math::Scale::new(1.0).expect("scale"));
        tree.set_clip_key(child, (1, Some(covering)));
        assert_eq!(tree.clip_key(child), Some((1, Some(covering))));
        tree.clear_clip_key(child);
        assert_eq!(tree.clip_key(child), None);
    }

    #[test]
    fn node_size_after_p06() {
        let size = size_of::<Node>();
        println!("size_of::<Node>() = {size}");
        assert!(
            size <= 1152,
            "Node 超预算：{size} B（备选 style: Box<Style>）"
        );
        assert!(
            size <= 1144,
            "P10 的预算是 1144（P06 的 1128 + interaction 8 + lane_offset 8）；超了要更新设计"
        );
    }
}

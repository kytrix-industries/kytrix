//! 持久节点树：`Node` / `NodeKind` / `LayoutBox` / `NodeTree`。
//!
//! 要点：
//! - 唯一的一棵树（I5）：节点树 + arena（`NodeId` 带代际）；**不维护全序平坦数组**
//!   （Chrome / Qt / Flutter 都不维护：绘制、命中、a11y、Tab 序全部由 `children` 的 DFS 遍历直接
//!   决定，结构插入/删除因此是 O(子树) 而不是 O(N) 拼接）；
//! - 所有写路径都走 `mark_*`（§9.3 的表 + `node::dirty` 的脏位表），`touch` 顺带并 damage 并清
//!   `REUSABLE`；
//! - **反向脏集合**（`dirty_bind` / `dirty_layout` / `dirty_paint` / `dirty_a11y` + 帧尾结算集合）
//!   让所有帧门禁是 O(1)、帧内工作与"变化量"成正比（Flutter `PipelineOwner` 模型）；
//! - `taffy::Cache` 与其余每节点旁表都在 `Node` 外（`layout_cache` 为按需 `HashMap`）。
//!
//! `Node` 的体积受 `node/tree/tests.rs` 的 `size_of` 断言约束（≤320 B）：冷字段
//! （`Style` / `StylePatch` / 文本载荷 / 依赖集合）全部装箱，热字段（dirty / damage / 布局盒 /
//! lane 状态）留在主存储里。

mod arena;
mod content;
mod dirty;
mod image;
mod layout;
#[cfg(test)]
mod tests;
mod traverse;

#[cfg(feature = "test-support")]
use std::cell::RefCell;
use std::{fmt, rc::Rc, sync::Arc};

use smallvec::SmallVec;
use smol_str::SmolStr;

use self::{content::TextSlot, layout::LayoutStats};
use crate::{
    error::EngineError,
    event::Action,
    interaction::{NodeInteraction, OverlaySlot, ScrollSlot, VirtualSlot},
    layout::{
        Axis, LayoutOverride, LayoutReport, MeasureFn,
        hit_test::{HitResult, HitShape},
    },
    math::{Edges, PhysicalRect, Point, Rect, Size, Transform},
    node::{
        dependency::ReadCtx,
        dirty::{DirtyFlags, DirtySet},
        id::{Arena, NodeId},
        image::ImageContent,
        signal::SignalId,
    },
    paint::{
        canvas::Canvas,
        geometry::GeometryId,
        scene::LaneId,
        style::{Clip, PaintSpec, Style, StylePatch},
    },
    resources::ResourceId,
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
/// 内部按平台无关的几何/画布形式存放（尚无平台窗口）。文本载荷装箱（`Box<TextContent>`）：
/// 它只出现在文本叶子上，却让每个容器节点白付 ~100 B。
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// 普通容器/叶子：由引擎按 `Style`/`StylePatch` 生成图元。
    Box,
    /// 自绘节点：挂载时提交的画布内容（P02 的 `Canvas`；`Rc` 共享，paint 只加引用计数）。
    Canvas(Arc<Canvas>),
    /// 静态几何节点：引用 P09 的 GPU 几何。
    Geometry(GeometryId),
    /// 文本叶子：载荷是 [`TextContent`]，布局由 `App::refresh_text` 注入的 measure 闭包完成。
    ///
    /// 在首次 `refresh_text` 之前它的 measure 为 `0×0`（`NodeBuilder::text` 不伪造闭包）。
    Text(Box<TextContent>),
    /// 图片叶子（E7）：载荷是 [`ImageContent`]（资源 + `fit` + 调制 + 内在尺寸）。
    ///
    /// 装箱理由同 `Text`：`ImageContent` 约 40 B，内联会把 `Node` 顶出尺寸预算（≤336 B）。
    /// 尺寸解析与 measure 安装由 `App::refresh_images` 在布局之前完成 —— 在首次刷新之前
    /// measure 为 `0×0`（与文本同型：`NodeBuilder::image` 不伪造与资源不一致的尺寸）。
    Image(Box<ImageContent>),
}

impl NodeKind {
    /// 叶子节点（`Canvas`/`Geometry`/`Text`/`Image`）不布局子节点 —— 消费者：
    /// `depends_on_children`、`NodeBuilder::build` 的 debug 断言。
    #[must_use]
    pub(crate) fn is_leaf(&self) -> bool {
        matches!(
            self,
            Self::Canvas(_) | Self::Geometry(_) | Self::Text(_) | Self::Image(_)
        )
    }

    /// 静态几何节点（唯一会被 per-draw `Transform` 影响的种类；命中走逆变换）。
    #[must_use]
    pub(crate) fn is_geometry(&self) -> bool {
        matches!(self, Self::Geometry(_))
    }

    /// 文本载荷（P06 的 paint 分派与 `App::refresh_text` 的唯一读入口）。
    #[must_use]
    pub(crate) fn text(&self) -> Option<&TextContent> {
        match self {
            Self::Text(content) => Some(content),
            _ => None,
        }
    }

    /// 图片载荷（E7：paint 分派、`App::refresh_images` 与 a11y 的唯一读入口）。
    #[must_use]
    pub(crate) fn image(&self) -> Option<&ImageContent> {
        match self {
            Self::Image(content) => Some(content),
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
    hit_shape: Option<HitShape>,
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

/// 空补丁的静态实例：`style_patch` 用 `Option<Box<StylePatch>>` 存（`None` = 空补丁），
/// 读路径回落到它，因此**每个节点都不为默认补丁付内联体积或一次分配**。
static EMPTY_STYLE_PATCH: StylePatch = StylePatch {
    background: None,
    foreground: None,
    border: None,
    opacity: None,
    visible: None,
    interactive: None,
    cursor: None,
    affects_layout: false,
};

/// 一个持久节点（跨帧存活）。**字段全私有**（I6：应用层不得持 `&mut Node`）。
///
/// 字段按"热 / 冷"分组：每帧被读的 `dirty` / `damage` / 布局盒 / lane 状态在主存储里，
/// 挂载后基本不变的 `Style` / `StylePatch` / 文本载荷 / 依赖集合被装箱（`Node` ≤ 320 B）。
pub struct Node {
    id: NodeId,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    kind: NodeKind,
    /// 静态结构（挂载时一次写入，之后只有结构变更才重写）。冷字段：装箱。
    style: Box<Style>,
    /// 动态属性补丁（`bind_style` 的**上次求值**结果）；`None` = 空补丁（见 `EMPTY_STYLE_PATCH`）。
    style_patch: Option<Box<StylePatch>>,
    /// 动态属性闭包（`None` = 静态节点）。
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    /// 自定义叶子测量（P05 的文本入口；见 [`MeasureFn`] 的纯函数契约）。
    measure: Option<MeasureFn>,
    /// 自定义命中形状（局部坐标；`None` = 用 `layout.bounds` 判定）。
    hit_shape: Option<Box<HitShape>>,
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
    ///
    /// **不再保存"上一次布局盒"**：damage 的旧一侧由 `place_subtree` 在写新盒时精确并入
    /// （`damage ∪ old ∪ new`），因此 `prev_layout` 是冗余的 32 B（R03-3 的瘦身项）。
    layout: LayoutBox,
    /// taffy 写入的**父相对** border box（`set_unrounded_layout` 的唯一落点）。
    local: Rect,
    /// taffy 写入的四边 border 宽度。
    local_border: Edges,
    /// 最近一次被 taffy 布局的 epoch（`place_subtree` 的「本轮是否新布局」判据）。
    laid_out_epoch: u32,
    /// 本帧累积的逻辑 damage AABB（§9.4）。
    damage: Rect,
    /// 该节点的绘制桶（`None` = 尚无绘制输出；**非几何节点恒 ≤1 条**）。
    ///
    /// 几何节点按块持有**多条** lane（X5，`paint/pass.rs` 的块 lane）：块 lane 在绘制序里
    /// 连续、块间序 = 块序，索引缓冲里的块区间也连续 ⇒ 块内/块间绘制序与单 lane 完全一致。
    /// 追加 K 线只重建尾块 lane，热力图改色只重建受影响块的 lane。
    lanes: SmallVec<[LaneId; 2]>,
    /// 上次求值实际读过的信号（差分清理用；`replace_deps` 的 `old`）。只有挂 `bind_*` 的节点非空。
    deps: Option<Box<SmallVec<[SignalId; 8]>>>,
    dirty: DirtyFlags,
    /// 交互槽（点击/拖拽声明、滚动、虚拟列表、浮层）；`None` = 非交互节点。
    ///
    /// 一个 `Box` 装下全部字段（`Node` 只 +8 B）。
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
            .field("style_patch", &self.style_patch())
            .field("bind_style", &self.bind_style.as_ref().map(|_| "<closure>"))
            .field("measure", &self.measure.as_ref().map(|_| "<closure>"))
            .field("hit_shape", &self.hit_shape)
            .field("layout_override", &self.layout_override)
            .field("paint", &self.paint)
            .field("content_offset", &self.content_offset)
            .field("text_bind", &self.text_bind.as_ref().map(|_| "<closure>"))
            .field("text_measured_size", &self.text_measured_size)
            .field("text_content_width", &self.text_content_width)
            .field("layout", &self.layout)
            .field("local", &self.local)
            .field("local_border", &self.local_border)
            .field("laid_out_epoch", &self.laid_out_epoch)
            .field("damage", &self.damage)
            .field("lanes", &self.lanes)
            .field("deps", &self.deps())
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
        self.style_patch.as_deref().unwrap_or(&EMPTY_STYLE_PATCH)
    }

    /// 失效集合（消费者：`App::refresh_styles`、`StylePassReport`）。
    #[must_use]
    pub fn dirty(&self) -> DirtyFlags {
        self.dirty
    }

    /// 节点自己的句柄（消费者：P08 的 `Probe`、诊断日志）。
    #[must_use]
    /// 节点句柄（含窗口归属；`arena` 铸出的原值）。
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// 重贴句柄（消费者：`NodeTree::bind_window` —— 树绑定窗口时重贴根节点的键）。
    ///
    /// 只有根节点会在绑定窗口前存在，因此这是**一次性**的初始化操作，不是可变身份。
    pub(crate) fn retag(&mut self, id: NodeId) {
        self.id = id;
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

    /// 该节点的**第一条**绘制桶（P06 的 paint 分配；`Probe` 读取）。
    ///
    /// 几何节点可能有多条块 lane；完整集合走 [`Node::lanes`]。
    #[must_use]
    pub fn lane(&self) -> Option<LaneId> {
        self.lanes.first().copied()
    }

    /// 该节点的全部绘制桶（非几何节点恒 ≤1 条；几何节点 = 块 lane，按块序）。
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
    pub fn hit_shape(&self) -> Option<&HitShape> {
        self.hit_shape.as_deref()
    }

    /// 布局覆盖（P10 的拖拽/分栏手柄；`None` = 由 taffy 决定）。
    #[must_use]
    pub fn layout_override(&self) -> Option<LayoutOverride> {
        self.layout_override
    }

    /// 是否挂了动态闭包（`bind_style` / `bind_text`）。消费者：悬停标脏（`app/input.rs`）
    /// 与 `Window::frame` 的 debug 断言（无 App 通道的窗口帧不跑样式/文本 pass）。
    #[must_use]
    pub(crate) fn has_bindings(&self) -> bool {
        self.bind_style.is_some() || self.text_bind.is_some()
    }

    /// 静态绘制属性（消费者：`paint/pass.rs`）。
    #[must_use]
    pub(crate) fn paint_spec(&self) -> Option<&PaintSpec> {
        self.paint.as_deref()
    }

    /// 上次求值读过的信号（`Option` = 从未求值 / 没有依赖）。
    #[must_use]
    pub(crate) fn deps(&self) -> Option<&[SignalId]> {
        self.deps.as_deref().map(SmallVec::as_slice)
    }

    /// 把当前布局盒并入 damage（§9.4）。
    ///
    /// 位置变化的"旧一侧"由 `place_subtree` 在写新盒时并入（`damage ∪ old ∪ new`），
    /// 这里只补当前盒。
    fn accumulate_damage(&mut self) {
        self.damage = self.damage.union(self.layout.bounds);
    }
}

/// 一次 paint 遍历所需的**只读**节点状态（单次 arena 查找算出全部，避免每节点 3–4 次重复查找）。
///
/// 消费者：`paint::pass::paint_node`。旧实现逐个访问器取数（`style` / `style_patch` / `dirty` /
/// `clip_key` / `lane_offset` / `lane_of` / `scroll_slot`），每个都各自做一次 arena 槽位查找 +
/// 选项解引用 —— 10k 节点每帧因此多付 ~1 ms（R03-10 的同源浪费）。
pub(crate) struct PaintNodeState {
    /// 自身不可见（`display: none` 或 `visible: false`）——调用方负责"祖先隐藏"。
    pub hidden: bool,
    /// lane 可整桶复用（`REUSABLE` + clip 上下文一致 + 滚动偏移戳一致）。
    pub reusable: bool,
    /// 节点的**第一条**绘制桶（`None` = 尚无绘制输出）。
    pub lane: Option<LaneId>,

    /// 几何内容（`None` = 非几何节点）：paint 走分块路径而不是普通单 lane 路径。
    pub geometry: Option<GeometryId>,
    /// 直接子节点数（循环里按下标取，不拷 `children`）。
    pub children: usize,
    /// 该节点 `Clip::Bounds` 的 padding box（屏幕坐标；`None` = 不裁剪子树）。
    pub clip_bounds: Option<Rect>,
    /// 该节点滚动槽的偏移（`None` = 非滚动容器）。
    ///
    /// `Some(_)` 表示**这个节点是滚动容器**（哪怕当前偏移恰好是 0）：paint pass 据此给它分配
    /// 一个合成端滚动槽位，内容按槽位偏移在绘制期平移，滚动因此不重建任何 lane。
    pub scroll: Option<Point>,
    /// 该节点的图元必须烘焙成**屏幕坐标**（自绘节点：几何 / Canvas）。
    ///
    /// `FrameOp::Geometry` 没有滚动槽字段、Canvas 图元也不参与合成端平移（应用给的是绝对
    /// 坐标），因此这两类节点的 lane 不能登记滚动槽：它们留在屏幕坐标里，祖先/自身滚动时
    /// 照旧重建（正确性优先，成本只落在自绘节点上）。
    pub screen_baked: bool,
}

/// 几何节点一条**块 lane** 上次烘焙时的块状态（X5）。
///
/// - `revision`：几何数据里该块的块版本。paint 拿它与快照的块版本比较：相等 ⇒ 该块 lane
///   可以整条复用（追加 K 线只动尾块时，前面的块一条都不重建）；不等 ⇒ 只重建这一条。
/// - `bounds`：该块的逻辑 AABB。重建时把**旧 bounds ∪ 新 bounds** 并入 damage（顶点位移时
///   旧位置也要重画）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BakedGeometryChunk {
    pub revision: u64,
    pub bounds: Rect,
}

/// paint 脏子树剪枝（A1）的可观测计数（验收 / 诊断面）。
///
/// 全部是真实观测值：`visited_nodes` 是最近一次 paint 遍历真正做节点状态判定的节点数
/// （全量帧 ≈ 节点总数；脏闭包增量帧 ≈ 脏节点数 × 树深），后三项是单调累计的帧数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaintCounters {
    /// 最近一帧 paint 遍历访问（做节点状态判定）的节点数。
    pub visited_nodes: usize,
    /// 累计：增量路径因新建 / 释放 lane 而用轻量 DFS 重建绘制序的次数（回退率观测面）。
    pub order_rebuilds: u64,
    /// 累计：走全量遍历的 paint 帧数。
    pub full_frames: u64,
    /// 累计：走脏闭包增量遍历的 paint 帧数（含没有节点被访问的纯 damage / 滚动帧）。
    pub incremental_frames: u64,
}

/// 唯一的一棵节点树（I5）。
///
/// 没有全序数组：任何"全树顺序"都由 [`NodeTree::for_each_dfs`] / [`NodeTree::dfs_ids`] 从
/// `children` 现算（DFS 先序）。反向脏集合与容器登记表让每帧只碰"变化的部分"。
pub struct NodeTree {
    arena: Arena<Node>,
    root: NodeId,
    /// 该树绑定的窗口（`None` = 未绑定：测试 / headless 直接构造的树，句柄为通配作用域）。
    ///
    /// 绑定后，铸出的每个 `NodeId` 都带窗口 tag；把 A 窗的句柄传给 B 窗的 API 会在
    /// `NodeTree::get` 处**确定性**返回 `StaleNode`（R03-7，不是"碰巧索引不同"）。
    window: Option<crate::window::WindowId>,
    /// 布局提交前的**设备像素吸附比例**（B7；`None` = 关闭）。
    ///
    /// 语义见 [`NodeTree::set_device_scale`]：只吸附提交给绘制的**边界**（`layout.bounds` 的
    /// 四边），taffy 的内部布局（`local` / 内容宽 / min-max 约束）保持未圆整。
    device_scale: Option<f32>,
    /// 命中测试因**不可逆变换 / 非有限几何**而跳过的节点次数（C11/C13 的诊断面）。
    ///
    /// `hit_test` 只有 `Option<HitResult>` 一个返回通道，"跳过"无法从结果里区分于"没命中"；
    /// 计数器让这类故障**可观测**（测试断言 + HUD/日志），而不是静默变成"点不动"。
    hit_test_skips: std::cell::Cell<u64>,
    /// 每节点上次布局的**未圆整**绝对原点（仅 `device_scale.is_some()` 时维护）。
    ///
    /// 吸附开启后，"父盒吸附后没变"不再蕴含"子树吸附后也没变"：父在同一个设备像素内移动
    /// 0.4dp，自己吸附回原值、而孙子的边界可能跨过半像素。这个旁表让 post-pass 的早期退出
    /// 判定同时看**未圆整**位移（8 B/节点；整数 DPI 下不分配，见 `set_device_scale`）。
    raw_origins: Vec<Point>,
    /// taffy 布局缓存（**按需**：只有真正参与过布局的节点才有条目）。
    ///
    /// 按 `NodeId`（含代际）索引，卸载即删除；`taffy::Cache` 约 0.5 KB，所以不能像旁表那样
    /// 按 arena 槽位预分配 —— 20 万节点静态树的槽位表会白付 ~100 MB（R03-3）。
    layout_cache: ahash::HashMap<NodeId, taffy::Cache>,
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
    a11y_slots: Vec<Option<Box<crate::a11y::tree::NodeA11y>>>,
    /// 图片节点「需要在布局前重解析内在尺寸」的标记（E7）。
    ///
    /// 置位点：图片节点挂载（`arena::insert_child`）、资源句柄替换（`set_image_resource`）。
    /// 消费点：`App::refresh_images`（`App::frame` 在布局之前取走，未置位时 O(1) 早退）。
    images_need_refresh: bool,
    /// 动态属性待重算（`BIND`）的节点集合。消费者：`App::refresh_styles`。
    dirty_bind: DirtySet,
    /// 待重排（`LAYOUT`）的节点集合。消费者：`layout_if_dirty` 门禁 + taffy 布局；
    /// 清除点是 `set_local_layout`。
    dirty_layout: DirtySet,
    /// 待重建 lane（`PAINT`）的节点集合。消费者：paint pass（`clear_paint` 清除）。
    dirty_paint: DirtySet,
    /// 待重发 a11y（`A11Y`）的节点集合（O(1) 的「有没有 a11y 工作」判据）。
    dirty_a11y: DirtySet,
    /// 帧尾结算集合：本帧动过（标了 `LAYOUT|PAINT` 或布局移动）的节点。
    ///
    /// 唯一消费者 + 清除点：`settle_frame`（damage 归零 / 置 `REUSABLE`）。
    /// 旧实现遍历整个全序数组，帧成本因此是 O(N)。
    dirty_settle: DirtySet,
    /// 本轮布局中**位置或尺寸真的变了**的节点（`mark_moved_a11y` 的输入）。
    /// 由 `place_subtree` 登记、布局 post-pass 之后消费 —— 不再为了补 a11y 而全树扫描。
    moved_nodes: DirtySet,
    /// 滚动容器的登记表（挂载时登记、卸载时剔除）。
    ///
    /// `App::clamp_scroll_offsets` 每帧要重夹所有滚动偏移，旧实现为此遍历全树；登记表让它是
    /// O(滚动容器数) —— 与节点数无关。
    scroll_containers: Vec<NodeId>,
    /// 虚拟列表容器的登记表（同上；`App::rewindow_virtuals` 每帧的输入）。
    virtual_containers: Vec<NodeId>,
    /// 浮层容器的登记表（同上；`App::resolve_overlay_placements` 每帧的输入）。
    ///
    /// 旧的实现每帧拷贝 root 的**全部子节点**再过滤 `overlay_slot`：10k 行的盘口表
    /// 因此每帧白付一次 10k 元素的收集 + 80 KB 分配。
    overlay_containers: Vec<NodeId>,
    /// 文本内容可能变化的节点集合（P1-2.3；`set_text_content` 登记、`refresh_text` 消费）。
    text_dirty: content::TextDirtyQueue,
    /// 「需要重建 `ShapedLine`」的文本节点集合（R12-14；`layout_text` 只遍历它）。
    text_layout_dirty: content::TextDirtyQueue,
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
    /// 几何节点的 per-draw 变换旁表（R05-12：`Primitive::Geometry.transform` 的来源）。
    /// 只在 `set_geometry_transform` 写入时分配条目，单位矩阵 = 无条目。
    geometry_transforms: ahash::HashMap<NodeId, Transform>,
    /// 几何节点的**块 lane 已烘焙状态**（X5；与 `Node.lanes` 一一对应）。
    ///
    /// 只在几何节点上有条目；paint 用它判断「哪些块的版本变了、只需要重建哪几条 lane」。
    geometry_chunks: ahash::HashMap<NodeId, SmallVec<[BakedGeometryChunk; 2]>>,
    /// 几何 id → 绑定它的节点（**反向索引**，X5）。
    ///
    /// 绑定节点 = 几何节点（`NodeKind::Geometry`）+ 内容里引用了该几何的 Canvas 节点
    /// （`Primitive::Geometry` / `Primitive::Path`）。几何数据 range 更新后据此 O(绑定数) 把
    /// 节点标 `PAINT`（不扫描全树）；节点卸载 / Canvas 内容替换时同步重建。
    geometry_bindings: ahash::HashMap<GeometryId, SmallVec<[NodeId; 1]>>,
    /// paint 脏子树剪枝的**祖先闭包**戳记（按 arena 槽位；`paint_epoch` 的当前代次在闭包内）。
    ///
    /// 每帧从 `dirty_paint` 快照向 root 走标记（`mark_paint_closure`），增量遍历只下行到
    /// 被标记的节点；与 `clip_keys` 同型（按槽位、懒扩容、卸载清空）。用代次而不是 `bool`
    /// 是为了每帧只付 O(脏 × 深度) 标记成本、不付 O(N) 清表成本。
    paint_marks: Vec<u32>,
    /// 当前闭包标记代次（`0` 保留为「没有本帧闭包」；每次标记 +1）。
    paint_epoch: u32,
    /// 每个滚动容器节点的**合成端槽位**（按 arena 槽位；`0` = 未分配）。
    ///
    /// Full 路径稠密编号时写入（入口先把全表清 0，隐藏 / 未访问节点因此自动失效）；
    /// Incremental 路径只读它把偏移写回 `scroll_slots`，**不重新编号**（结构未变 ⇒ 编号稳定）。
    /// 缓存失效（`0`）时增量路径追加新槽，而不是重排已有编号（已烘焙进 lane 的槽号不动）。
    scroll_slot_of: Vec<u32>,
    /// 结构代次：`mark_structure` 每次真的改结构时 +1（增量帧的硬全量判据）。
    structure_epoch: u32,
    /// 上一次全量 paint 看到的结构代次。
    painted_structure_epoch: u32,
    /// 滚动容器**登记**代次（`note_scroll_container` +1）：运行期新增滚动容器 ⇒ 槽位表必须重建。
    scroll_reg_epoch: u32,
    /// 上一次全量 paint 看到的滚动容器登记代次。
    painted_scroll_reg_epoch: u32,
    /// 强制下一次 paint 走全量路径（仅 `force_repaint_all` 置位；分类时消费一次）。
    force_full_paint: bool,
    /// paint 剪枝计数（A1 的验收面；见 [`PaintCounters`]）。
    paint_visited: usize,
    paint_order_rebuilds: u64,
    paint_full_frames: u64,
    paint_incremental_frames: u64,
    /// 消费脏集合时把条目搬过来再遍历的复用缓冲（`mem::take` / `recycle` 成对使用，保容量）。
    dirty_scratch: Vec<NodeId>,
    /// 本帧"被引擎簿记访问过的不同节点"集合（仅 `test-support`；`FrameRecord::scanned_nodes`）。
    ///
    /// 用 `RefCell` 而不是 `&mut`：DFS 遍历函数只拿 `&self`，而计数不应改变借用形状。
    #[cfg(feature = "test-support")]
    scanned: RefCell<ahash::HashSet<NodeId>>,
    /// 挂了动态闭包（`bind_style` / `bind_text`）的节点数（O(1)：`Window::frame` 的 debug 断言）。
    bound_nodes: usize,
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
    /// 把树绑定到窗口（`App::create_window` 在 `Window::new` 之后立刻调用）。
    ///
    /// 绑定后铸出的句柄带窗口身份：跨窗口误用会在 `get` 处确定性返回 `StaleNode`（R03-7）。
    pub(crate) fn bind_window(&mut self, window: crate::window::WindowId) {
        arena::bind_window(self, window)
    }
    /// 该树绑定的窗口（`None` = 未绑定：测试直接构造的树）。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn window(&self) -> Option<crate::window::WindowId> {
        self.window
    }
    /// 树的窗口 tag（`0` = 未绑定；taffy 回调补全句柄身份用）。
    #[must_use]
    pub(crate) fn window_tag(&self) -> u32 {
        arena::window_tag(self)
    }
    /// 设置**布局提交前的设备像素吸附**比例（B7）。
    ///
    /// 语义（只吸附"边界"，不吸附"内部尺寸"）：
    /// - `layout.bounds` 的四条边界各自 `round(dp × scale) / scale`，宽高由吸附后的边界相减；
    /// - taffy 的 `local`（父相对几何）、内容宽（文本断行依据）、min/max 约束**不**吸附 ——
    ///   它们决定"盒子里装什么"，圆整它们会让排版内部与提交给绘制的盒子两套真相；
    /// - 递归引用系用未圆整坐标，因此嵌套深度不会累积舍入误差；
    /// - `None` = 关闭（未绑定窗口的树 / 测试构造的树；保持"taffy 不做圆整"的原始语义）。
    ///
    /// 关闭态存在的理由：`scale == 1` 时逻辑像素就是设备像素，引擎的既有契约
    /// （P01 §7.5）明确"布局不做圆整"、亚像素位置由绘制期 AA 处理；像素吸附解决的是
    /// **非整数 DPI** 下"1px 逻辑边框变成 1.25/1.5px 物理边"的边界错位（表格分割线、面板
    /// 边框发虚）。窗口层只在 `scale != 1` 时开启它（见 `window::scale`）。
    pub(crate) fn set_device_scale(&mut self, scale: Option<f32>) {
        layout::set_device_scale(self, scale)
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
    /// 实现见子模块 `arena::append_child`（本文件只保留签名）。
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), EngineError> {
        arena::append_child(self, parent, child)
    }
    /// 存活节点数（O(1)；消费者：probe / 诊断 / 遍历容量预估）。
    #[must_use]
    pub fn len(&self) -> usize {
        arena::len(self)
    }
    /// 是否只有根节点（`clippy::len_without_is_empty` 的配对）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() <= 1
    }

    // ── traverse：实现见 `traverse` 子模块的自由函数 ──
    /// 实现见子模块 `traverse::for_each_dfs`（本文件只保留签名）。
    pub(crate) fn for_each_dfs(&self, visit: impl FnMut(NodeId)) {
        traverse::for_each_dfs(self, visit)
    }
    /// 实现见子模块 `traverse::dfs_ids`（本文件只保留签名）。
    #[must_use]
    pub(crate) fn dfs_ids(&self) -> Vec<NodeId> {
        traverse::dfs_ids(self)
    }
    /// 实现见子模块 `traverse::for_each_subtree`（本文件只保留签名）。
    pub(crate) fn for_each_subtree(&self, root: NodeId, visit: impl FnMut(NodeId)) {
        traverse::for_each_subtree(self, root, visit)
    }
    #[must_use]
    pub(crate) fn is_visible_locally(&self, node: NodeId) -> bool {
        traverse::is_visible_locally(self, node)
    }
    #[must_use]
    pub(crate) fn is_effectively_visible(&self, node: NodeId) -> bool {
        traverse::is_effectively_visible(self, node)
    }
    /// 该节点及其祖先都未被 `interactive: Some(false)` 禁用（命中/焦点/Action 共用的继承规则）。
    ///
    /// 详见子模块 `content::is_effectively_interactive` 的文档（含与 CSS `pointer-events` 的差别）。
    #[must_use]
    pub(crate) fn is_effectively_interactive(&self, node: NodeId) -> bool {
        content::is_effectively_interactive(self, node)
    }
    /// 记一次「命中因不可逆变换 / 非有限几何而跳过」（C11/C13 诊断）。
    pub(crate) fn note_hit_test_skip(&self) {
        content::note_hit_test_skip(self)
    }
    /// 累计的命中跳过次数（单调递增；诊断 / 测试断言用）。
    #[must_use]
    pub fn hit_test_skips(&self) -> u64 {
        content::hit_test_skips(self)
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
    /// 实现见子模块 `traverse::note_scroll_container`（本文件只保留签名）。
    pub(crate) fn note_scroll_container(&mut self, node: NodeId) {
        traverse::note_scroll_container(self, node)
    }
    /// 实现见子模块 `traverse::note_virtual_container`（本文件只保留签名）。
    pub(crate) fn note_virtual_container(&mut self, node: NodeId) {
        traverse::note_virtual_container(self, node)
    }
    /// 实现见子模块 `traverse::note_overlay_container`（本文件只保留签名）。
    pub(crate) fn note_overlay_container(&mut self, node: NodeId) {
        traverse::note_overlay_container(self, node)
    }
    /// 实现见子模块 `traverse::scroll_containers`（本文件只保留签名）。
    #[must_use]
    pub(crate) fn scroll_containers(&self) -> &[NodeId] {
        &self.scroll_containers
    }
    /// 实现见子模块 `traverse::virtual_containers`（本文件只保留签名）。
    #[must_use]
    pub(crate) fn virtual_containers(&self) -> &[NodeId] {
        &self.virtual_containers
    }
    /// 实现见子模块 `traverse::overlay_containers`（本文件只保留签名）。
    #[must_use]
    pub(crate) fn overlay_containers(&self) -> &[NodeId] {
        &self.overlay_containers
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
    /// 仅测试：全树重算子树包围盒（与 `place_subtree` 的增量维护做逐位一致性校验）。
    #[cfg(test)]
    pub(crate) fn recompute_subtree_bounds(&mut self) {
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
    pub(crate) fn set_layout_for_test(&mut self, node: NodeId, layout: LayoutBox) -> bool {
        layout::set_layout_for_test(self, node, layout)
    }
    /// 实现见子模块 `layout::layout_cache`（本文件只保留签名；测试读点）。
    #[cfg(test)]
    pub(crate) fn layout_cache(&self, node: NodeId) -> Option<&taffy::Cache> {
        layout::layout_cache(self, node)
    }

    // ── dirty：实现见 `dirty` 子模块的自由函数 ──
    pub(crate) fn mark_bind(&mut self, node: NodeId) -> bool {
        dirty::mark_bind(self, node)
    }
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
    /// 整棵子树标 `PAINT | A11Y`（并清 `REUSABLE`）。
    ///
    /// 消费者：`App::request_node_repaint`（R01-16 的节点级重绘：子树重建 lane）。
    pub(crate) fn mark_subtree_paint(&mut self, node: NodeId) -> usize {
        dirty::mark_subtree_paint(self, node)
    }
    pub(crate) fn clear_bind(&mut self, node: NodeId) -> bool {
        dirty::clear_bind(self, node)
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
    #[must_use]
    pub(crate) fn a11y_dirty(&self) -> usize {
        dirty::a11y_dirty(self)
    }
    fn touch(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        dirty::touch(self, node, flags)
    }
    fn touch_layout_only(&mut self, node: NodeId) -> bool {
        dirty::touch_layout_only(self, node)
    }
    /// 本帧是否有待布局的节点（O(1)；`layout_if_dirty` 的门禁）。
    #[must_use]
    pub(crate) fn has_layout_work(&self) -> bool {
        !self.dirty_layout.is_empty()
    }
    /// 本帧是否有待重建 lane 的节点（O(1)；`needs_node_repaint` 的门禁）。
    #[must_use]
    pub(crate) fn has_paint_work(&self) -> bool {
        !self.dirty_paint.is_empty()
    }

    // ── paint 剪枝（A1）：帧分类输入 / 祖先闭包 / 滚动槽缓存 / 计数 ──────────────
    /// paint 剪枝的计数快照（验收 / 诊断；只读、不改变状态）。
    #[must_use]
    pub fn paint_counters(&self) -> PaintCounters {
        PaintCounters {
            visited_nodes: self.paint_visited,
            order_rebuilds: self.paint_order_rebuilds,
            full_frames: self.paint_full_frames,
            incremental_frames: self.paint_incremental_frames,
        }
    }

    /// 本帧 paint 开始时的**硬全量**判据：结构 / 滚动容器登记代际变化，或强制全量标志。
    ///
    /// 返回 `true` ⇒ 必须走全量遍历（脏节点可能新建 / 丢失 lane，滚动槽必须重新稠密编号，
    /// 绘制序必须重建）。恒消费 `force_full_paint`；「已见代际」的写回在
    /// [`NodeTree::note_paint_frame`]（只有真的走了全量才提交，避免判据丢失）。
    pub(crate) fn paint_epoch_changed(&mut self) -> bool {
        let forced = std::mem::take(&mut self.force_full_paint);
        forced
            || self.structure_epoch != self.painted_structure_epoch
            || self.scroll_reg_epoch != self.painted_scroll_reg_epoch
    }

    /// 记录本帧 paint 走的是哪条路径（计数 + 全量帧的已见代际提交）。
    pub(crate) fn note_paint_frame(&mut self, full_path: bool, visited: usize) {
        self.paint_visited = visited;
        if full_path {
            self.paint_full_frames = self.paint_full_frames.saturating_add(1);
            self.painted_structure_epoch = self.structure_epoch;
            self.painted_scroll_reg_epoch = self.scroll_reg_epoch;
        } else {
            self.paint_incremental_frames = self.paint_incremental_frames.saturating_add(1);
        }
    }

    /// 本帧没有跑 paint 遍历（a11y-only / 纯滚动快路径早期返回）：本帧访问数归零。
    pub(crate) fn reset_paint_visited(&mut self) {
        self.paint_visited = 0;
    }

    /// 记一次「增量路径用轻量 DFS 重建绘制序」（`rebuild_order_only` 的计数）。
    pub(crate) fn note_order_rebuild(&mut self) {
        self.paint_order_rebuilds = self.paint_order_rebuilds.saturating_add(1);
    }

    /// 快照本帧的 `PAINT` 脏节点（**不**清空集合；消费方必须 `recycle` 归还缓冲）。
    ///
    /// 与其它 `take_*` 一致：把条目记进 `test-support` 的 `scanned_nodes`（它们是引擎簿记
    /// 访问过的节点），但**不**把闭包祖先算进去 —— 闭包规模由
    /// [`PaintCounters::visited_nodes`] 单独观测。
    pub(crate) fn take_paint_work(&mut self) -> Vec<NodeId> {
        let NodeTree {
            dirty_scratch,
            dirty_paint,
            ..
        } = self;
        let mut out = std::mem::take(dirty_scratch);
        dirty_paint.snapshot_into(&mut out);
        #[cfg(feature = "test-support")]
        for node in &out {
            self.note_scanned(*node);
        }
        out
    }

    /// 从脏节点集合向 root 走，标记**祖先闭包**（含自身；重复命中即停）。
    ///
    /// 复杂度 O(脏 × 树深)，与节点总数无关。用代次戳记（`paint_marks` / `paint_epoch`）：
    /// 每帧只递增代次，不付 O(N) 清表成本。
    pub(crate) fn mark_paint_closure(&mut self, dirty: &[NodeId]) {
        self.paint_epoch = self.paint_epoch.wrapping_add(1);
        if self.paint_epoch == 0 {
            // u32 回绕（约 43 亿帧一次）：清表后从 1 重新开始，代价可忽略。
            self.paint_marks.fill(0);
            self.paint_epoch = 1;
        }
        let epoch = self.paint_epoch;
        for start in dirty.iter().copied() {
            let mut cursor = Some(start);
            while let Some(node) = cursor {
                let index = node.index() as usize;
                if self.paint_marks.len() <= index {
                    self.paint_marks.resize(index + 1, 0);
                }
                if self.paint_marks[index] == epoch {
                    break;
                }
                self.paint_marks[index] = epoch;
                cursor = self.arena.get(node).and_then(|target| target.parent);
            }
        }
    }

    /// 该节点是否在本帧的脏闭包内（`mark_paint_closure` 之后有效；`0` 代次恒为假）。
    #[must_use]
    pub(crate) fn paint_marked(&self, node: NodeId) -> bool {
        self.paint_epoch != 0
            && self
                .paint_marks
                .get(node.index() as usize)
                .is_some_and(|stamp| *stamp == self.paint_epoch)
    }

    /// 强制下一次 paint 走全量路径（仅 `test-support`：oracle 的「全量」对照路径）。
    ///
    /// 与 [`NodeTree::force_repaint_all`] 的区别：这里**只**改帧分类，不把节点塞进脏集合 ——
    /// 差分 oracle 因此能比较「同一脏集合、同一状态」下的增量与全量产物的逐字段一致性。
    #[cfg(feature = "test-support")]
    pub(crate) fn force_full_paint(&mut self) {
        self.force_full_paint = true;
    }

    /// 全量 paint 入口：作废全部节点的滚动槽缓存（隐藏 / 未访问节点因此必须重新分配）。
    pub(crate) fn begin_full_scroll_slots(&mut self) {
        self.scroll_slot_of.fill(0);
    }

    /// 节点的缓存滚动槽（`0` = 未分配）。
    #[must_use]
    pub(crate) fn scroll_slot_cached(&self, node: NodeId) -> u32 {
        self.scroll_slot_of
            .get(node.index() as usize)
            .copied()
            .unwrap_or(0)
    }

    /// 写节点的缓存滚动槽（全量稠密编号 / 增量追加新槽时写入）。
    pub(crate) fn set_scroll_slot(&mut self, node: NodeId, slot: u32) {
        let index = node.index() as usize;
        if self.scroll_slot_of.len() <= index {
            self.scroll_slot_of.resize(index + 1, 0);
        }
        self.scroll_slot_of[index] = slot;
    }

    /// 本帧是否有待重算动态属性的节点（O(1)；`needs_frame` 的门禁）。
    #[must_use]
    pub(crate) fn has_bind_work(&self) -> bool {
        !self.dirty_bind.is_empty()
    }

    /// 实现见子模块 `dirty::take_dirty_bind`（本文件只保留签名）。
    pub(crate) fn take_dirty_bind(&mut self) -> Vec<NodeId> {
        dirty::take_dirty_bind(self)
    }
    /// 实现见子模块 `dirty::take_settle`（本文件只保留签名）。
    pub(crate) fn take_settle(&mut self) -> Vec<NodeId> {
        dirty::take_settle(self)
    }
    /// 实现见子模块 `dirty::take_a11y`（本文件只保留签名）。
    pub(crate) fn take_a11y(&mut self) -> Vec<NodeId> {
        dirty::take_a11y(self)
    }
    /// 实现见子模块 `dirty::take_moved`（本文件只保留签名）。
    pub(crate) fn take_moved(&mut self) -> Vec<NodeId> {
        dirty::take_moved(self)
    }
    /// 归还 [`NodeTree::take_dirty_bind`] 等借用走的复用缓冲（容量保留，避免每帧分配）。
    pub(crate) fn recycle(&mut self, mut buffer: Vec<NodeId>) {
        buffer.clear();
        self.dirty_scratch = buffer;
    }
    /// 是否还有节点挂着动态闭包（O(1)；`Window::frame` 的 debug 断言，取代旧的全树扫描）。
    #[must_use]
    pub(crate) fn has_bound_nodes(&self) -> bool {
        self.bound_nodes > 0
    }
    /// 仅测试：把某节点的位与反向集合彻底清空（测试的"从干净状态开始"）。
    #[cfg(test)]
    pub(crate) fn clear_all_dirty_for_test(&mut self) {
        dirty::clear_all_dirty_for_test(self)
    }
    /// 仅测试：直接设置失效位（不经过 mark_* 的副作用）。
    #[cfg(test)]
    pub(crate) fn set_dirty_for_test(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        dirty::set_dirty_for_test(self, node, flags)
    }
    /// 记录"引擎簿记访问过该节点"（仅 `test-support`；`FrameRecord::scanned_nodes` 的输入）。
    #[cfg(feature = "test-support")]
    pub(crate) fn note_scanned(&self, node: NodeId) {
        self.scanned.borrow_mut().insert(node);
    }
    /// 本帧被簿记访问过的不同节点数（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn scanned_nodes(&self) -> usize {
        self.scanned.borrow().len()
    }
    /// 帧初重置扫描计数（仅 `test-support`；`Window::begin_frame` 调用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn reset_scanned_nodes(&self) {
        self.scanned.borrow_mut().clear();
    }

    // ── content：实现见 `content` 子模块的自由函数 ──
    pub(crate) fn text_content(&self, node: NodeId) -> Option<&TextContent> {
        content::text_content(self, node)
    }
    /// 替换 Canvas 节点的画布内容（D-6 异步图片交付入口；语义见 `content::set_canvas`）。
    ///
    /// 同时**重建该节点的几何反向绑定**（旧内容引用的 `GeometryId` 解绑、新内容的重新绑定），
    /// 几何数据 range 更新后该 Canvas 节点因此会自动标脏。
    pub(crate) fn set_canvas(
        &mut self,
        node: NodeId,
        canvas: crate::paint::canvas::Canvas,
    ) -> bool {
        content::set_canvas(self, node, canvas)
    }

    // ── image（E7）：实现见 `image` 子模块的自由函数 ──
    /// 取走「图片节点需要在布局前重解析内在尺寸」的标记（`App::refresh_images` 的 O(1) 门禁）。
    pub(crate) fn take_images_need_refresh(&mut self) -> bool {
        std::mem::take(&mut self.images_need_refresh)
    }
    /// 解析所有图片节点的内在尺寸并安装 measure（返回尺寸真的变了的节点数）。
    pub(crate) fn refresh_image_intrinsics(
        &mut self,
        resources: &crate::resources::ResourceCache,
        scale: f32,
    ) -> usize {
        image::refresh_intrinsics(self, resources, scale)
    }
    /// 把异步解码交付的 `Ready` 结果写回图片节点（返回改写的节点数）。
    pub(crate) fn resolve_pending_images(
        &mut self,
        resolved: &ahash::HashMap<
            crate::resources::PendingResource,
            crate::resources::ResourceImage,
        >,
        scale: f32,
    ) -> usize {
        image::resolve_pending(self, resolved, scale)
    }
    /// 标记所有引用 `resource` 的图片节点重绘（资源释放后的失效入口）。
    pub(crate) fn invalidate_images_with_resource(&mut self, resource: ResourceId) -> usize {
        image::invalidate_with_resource(self, resource)
    }
    /// 标记所有引用「已不在缓存里」的资源的图片节点重绘（关窗释放后的悬垂防护）。
    pub(crate) fn invalidate_dead_images(
        &mut self,
        resources: &crate::resources::ResourceCache,
    ) -> usize {
        image::invalidate_dead(self, resources)
    }
    /// 替换图片节点的资源句柄（E7：DPI 换档 / 换图的公开入口）。
    ///
    /// 新资源的内在尺寸在下一次 `App::frame` 的布局前重解析（尺寸可能变），同时标
    /// `LAYOUT | PAINT` 保证该帧一定出现。返回 `false` = 节点不是图片节点。
    ///
    /// # Errors
    /// [`EngineError::StaleNode`]：节点不存在 / 已卸载。
    pub fn set_image_resource(
        &mut self,
        node: NodeId,
        resource: ResourceId,
    ) -> Result<bool, EngineError> {
        self.get(node)?;
        Ok(image::set_resource(self, node, resource))
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
    pub(crate) fn mark_text_layout_dirty(&mut self, node: NodeId) {
        content::mark_text_layout_dirty(self, node)
    }
    pub(crate) fn take_text_layout_dirty(&mut self, out: &mut Vec<NodeId>) {
        content::take_text_layout_dirty(self, out)
    }
    #[cfg(test)]
    pub(crate) fn text_layout_dirty_len(&self) -> usize {
        content::text_layout_dirty_len(self)
    }
    pub(crate) fn invalidate_text_lines(&mut self) -> usize {
        content::invalidate_text_lines(self)
    }
    /// 让所有 `Canvas` 节点的 lane 重建（图集换代后的 canvas 编码期安全网入口；v2.1 E1b）。
    pub(crate) fn mark_canvas_nodes_paint(&mut self) -> usize {
        content::mark_canvas_nodes_paint(self)
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
    #[cfg(test)]
    pub(crate) fn install_text_line_for_test(
        &mut self,
        node: NodeId,
        content: &crate::text::TextContent,
        line: Rc<ShapedLine>,
        width: f32,
    ) -> bool {
        content::install_text_line_for_test(self, node, content, line, width)
    }
    pub(crate) fn set_text_line(
        &mut self,
        node: NodeId,
        line: Rc<ShapedLine>,
        width: f32,
        generation: u32,
    ) -> bool {
        content::set_text_line(self, node, line, width, generation)
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
    /// 实现见子模块 `content::text_byte_index_at`（本文件只保留签名）。
    pub(crate) fn text_byte_index_at(
        &self,
        node: NodeId,
        local_point: Point,
    ) -> Result<usize, EngineError> {
        content::text_byte_index_at(self, node, local_point)
    }
    /// 让单个文本节点的 ShapedLine 失效并进入重排队列（D-5 绘制前硬校验用）。
    pub(crate) fn invalidate_text_line(&mut self, node: NodeId) {
        content::invalidate_text_line(self, node)
    }

    pub(crate) fn invalidate_text_layout(&mut self, node: NodeId) -> bool {
        content::invalidate_text_layout(self, node)
    }
    pub(crate) fn mark_all_text_dirty(&mut self) {
        content::mark_all_text_dirty(self)
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
    /// 几何节点的 per-draw 变换（R05-12；单位矩阵 = 未设置）。
    ///
    /// 消费者：`paint/pass.rs` 生成 `Primitive::Geometry` 时写入 `GeometryDraw.transform`，
    /// 编码器据此算 `FrameOp::Geometry.bounds`（变换后的 AABB ∩ clip）。
    #[must_use]
    pub(crate) fn geometry_transform(&self, node: NodeId) -> Transform {
        content::geometry_transform(self, node)
    }
    /// 是否存在任何 per-draw 几何变换（O(1)；命中测试的快路径判定）。
    #[must_use]
    pub(crate) fn has_geometry_transforms(&self) -> bool {
        content::has_geometry_transforms(self)
    }
    /// 设置几何节点的 per-draw 变换（顶点**不**重建：图表平移只改一个 uniform）。
    ///
    /// 标 `PAINT`（lane 需要重建，因为变换烘在 lane 的图元里）；`extra_damage` 由调用方
    /// （`App::set_geometry_transform`）给出「旧变换 ∪ 新变换下的可见范围」，与节点盒一样并进
    /// 节点的 damage 累加器（同一条 damage 路径，AR2 P1-D4）。
    ///
    /// 唯一的生产调用者是 `app::geometry::set_geometry_transform`；测试可直接调。
    ///
    /// # Errors
    /// [`EngineError::StaleNode`]：节点不存在或不是几何节点。
    pub(crate) fn set_geometry_transform(
        &mut self,
        node: NodeId,
        transform: Transform,
        extra_damage: Rect,
    ) -> Result<(), EngineError> {
        content::set_geometry_transform(self, node, transform, extra_damage)
    }
    pub(crate) fn content_offset(&self, node: NodeId) -> Point {
        content::content_offset(self, node)
    }
    pub(crate) fn lane_of(&self, node: NodeId) -> Option<LaneId> {
        content::lane_of(self, node)
    }
    /// lane 条数（几何节点 = 块数；消费者：paint 的分块路径 / 绘制序兜底重建）。
    pub(crate) fn lane_count(&self, node: NodeId) -> usize {
        content::lane_count(self, node)
    }
    /// 第 `index` 条 lane（越界 = `None`；消费者：paint 的绘制序兜底 / 隐藏子树 / 卸载回收）。
    pub(crate) fn lane_at(&self, node: NodeId, index: usize) -> Option<LaneId> {
        content::lane_at(self, node, index)
    }
    /// 把 lane 集合改成**单条**（普通节点的唯一写法）。
    pub(crate) fn set_lane(&mut self, node: NodeId, lane: LaneId) {
        content::set_lane(self, node, lane)
    }
    /// 追加一条 lane（几何节点的块 lane；按块序）。
    pub(crate) fn push_lane(&mut self, node: NodeId, lane: LaneId) {
        content::push_lane(self, node, lane)
    }
    /// 该几何节点的块 lane 烘焙状态（`None` = 不是几何节点 / 还没画过）。
    pub(crate) fn baked_geometry_chunks(
        &self,
        node: NodeId,
    ) -> Option<&SmallVec<[BakedGeometryChunk; 2]>> {
        content::baked_geometry_chunks(self, node)
    }
    /// 重置块 lane 烘焙状态的长度（新建 / 收缩时调用）。
    pub(crate) fn resize_baked_geometry_chunks(&mut self, node: NodeId, len: usize) {
        content::resize_baked_geometry_chunks(self, node, len)
    }
    /// 写一条块 lane 的烘焙状态。
    pub(crate) fn set_baked_geometry_chunk(
        &mut self,
        node: NodeId,
        chunk: usize,
        baked: BakedGeometryChunk,
    ) {
        content::set_baked_geometry_chunk(self, node, chunk, baked)
    }

    /// 截断 lane 集合到 `len`（几何节点的块数变化；多余 lane 必须由调用方先 release）。
    pub(crate) fn truncate_lanes(&mut self, node: NodeId, len: usize) {
        content::truncate_lanes(self, node, len)
    }
    /// 把绑定到 `geometry` 的节点标 `PAINT`（几何数据 range 更新后由 `App` 调用）。
    ///
    /// 绑定节点有两类：几何节点（`NodeKind::Geometry`）与内容里引用了该几何的 Canvas 节点。
    /// 对**几何节点**：**不清 `REUSABLE`、不并节点盒 damage** —— 几何数据的变化范围由块 bounds
    /// 精确给出（`paint` 在块 lane 重建时按旧∪新 bounds 记 damage），节点盒与其它绘制输出都
    /// 没变，保守清 `REUSABLE` 会把整节点（含全部块 lane）都拖去重建。
    /// 对 **Canvas 节点**：内容按几何引用整体重新编码，因此走 `mark_paint`（清 `REUSABLE`、
    /// 重建整条 lane、damage 按 Canvas 内容范围记）。
    ///
    /// 返回标脏的节点数（`0` = 该几何没有任何绑定节点）。
    pub(crate) fn mark_geometry_updated(&mut self, geometry: GeometryId) -> usize {
        content::mark_geometry_updated(self, geometry)
    }
    /// 注册「几何 id → 引用它的节点」绑定（消费者：`arena::insert_child` 的几何 / Canvas 分支）。
    pub(crate) fn bind_geometry_node(&mut self, geometry: GeometryId, node: NodeId) {
        content::bind_geometry_node(self, geometry, node)
    }
    pub(crate) fn lane_offset(&self, node: NodeId) -> Point {
        content::lane_offset(self, node)
    }
    pub(crate) fn set_lane_offset(&mut self, node: NodeId, offset: Point) {
        content::set_lane_offset(self, node, offset)
    }
    pub(crate) fn clip_key(&self, node: NodeId) -> Option<(u8, Option<PhysicalRect>)> {
        content::clip_key(self, node)
    }
    /// 一次取全 paint 所需的只读状态（消费者：`paint::pass::paint_node`）。
    ///
    /// `screen_offset` = 完整祖先滚动位移（屏幕 = 内容 − 它）；`bake_offset` = 图元烘焙偏移
    /// （不含本节点所属滚动槽自己的位移，见 `paint/pass.rs` 的 `paint_node`）。
    pub(crate) fn paint_state(
        &self,
        node: NodeId,
        screen_offset: Point,
        bake_offset: Point,
        clip_key: (u8, Option<PhysicalRect>),
    ) -> Option<PaintNodeState> {
        content::paint_state(self, node, screen_offset, bake_offset, clip_key)
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
    pub(crate) fn force_repaint_all(&mut self) {
        content::force_repaint_all(self)
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
        self.arena.get(node).and_then(Node::deps)
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
}

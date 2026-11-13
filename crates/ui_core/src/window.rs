//! 窗口：句柄、建窗参数、节点树 + 绘制场景的持有者。
//!
//! 设计见 `design/phase-03-node-state.md` §5.2 / §15。P03 是 headless 的：只保存参数，不驱动平台
//! （平台窗口句柄、DPI 事件、标题栏属 P12）。

use smol_str::SmolStr;

#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameRecord, FrameStats, PaintLog};
use crate::{
    error::EngineError,
    interaction::{FocusScope, PointerState, scroll},
    layout::LayoutReport,
    math::{Point, Rect, Scale, Size},
    node::{dirty::DirtyFlags, id::NodeId, tree::NodeTree},
    paint::{
        color::{ColorResolver, Interpolation},
        instance::{InstanceBuffer, InstanceEncoder},
        pass::{PaintPass, PaintReport},
        scene::Scene,
    },
    render::{
        backend::Renderer,
        compositor::{FramePlan, Plan, plan},
        damage::{DamageAccumulator, DamageSummary},
        timer::FrameTimings,
    },
};

/// 窗口句柄：单调递增、**永不复用**（陈旧 id 落回 `None`，因此不需要代际）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(u32);

impl WindowId {
    /// crate 内构造（消费者：`App::create_window`）。
    ///
    /// **0 不是合法句柄**：`App::create_window` 从 1 起单调递增，`WindowId(0)` 永不产生
    /// （`Option<WindowId>` 因此可以靠 `get` 返回 `None` 表达「窗口不存在」，无需哨兵值）。
    pub(crate) fn from_raw(value: u32) -> Self {
        debug_assert!(value != 0, "WindowId 0 是非法句柄");
        Self(value)
    }

    /// 原始值（诊断/日志）。
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0
    }

    /// 仅测试：直接构造。
    #[cfg(test)]
    pub(crate) const fn for_test(value: u32) -> Self {
        Self(value)
    }
}

/// 建窗参数。P03 只保存，不驱动平台（P12 才消费）。
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOptions {
    /// 标题（`SmolStr`，AGENTS §2.2）。
    pub title: SmolStr,
    /// 初始逻辑尺寸。
    pub size: Size,
    /// 初始 DPI 比例（由平台在 P12 传入；headless 默认 `Scale::ONE`）。
    pub scale: Scale,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: SmolStr::new_static(""),
            size: Size::new(800.0, 600.0),
            scale: Scale::ONE,
        }
    }
}

/// 单调递增的提交序号。**0 = 从未提交**（`Default`），首帧 = 1。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameIndex(u32);

impl FrameIndex {
    /// 原始值（`0` 只在 `Default` 里出现；`commit` 保证提交序号 ≥ 1）。
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// crate 内构造（渲染器的**一次性**帧，如快照；不参与窗口的 `FrameScheduler`）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn from_raw(value: u32) -> Self {
        debug_assert!(value >= 1, "FrameIndex 0 从未提交");
        Self(value)
    }
}

/// headless 版帧状态机（设计 §3.5 的 10 态在一次同步调用内部，见 F-66）。
///
/// crate 私有：P06 没有公开的观察者（测试用 `FrameOutcome`/`frame_index` 断言）；P09 的事件循环
/// 需要时再决定是否公开。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameState {
    /// 没有待办的帧。
    Idle,
    /// 已请求一帧，尚未开始。
    Pending,
    /// 正在跑 pass（`begin` 之后、`commit`/`abort` 之前）。
    Draining,
}

/// 一帧的最终结果（`FrameRecord` 只存在于 `test-support`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// 本帧没有像素要画：不跑 pass、不编码、不提交、`FrameIndex` 不变（I3）。
    Idle,
    /// 已提交；序号 = 本次提交后的 `FrameIndex`。
    Committed(FrameIndex),
}

/// `Draining` 的凭证：只能由 [`FrameScheduler::begin`] 铸出，`commit`/`abort` 各消费一次。
pub(crate) struct FrameTicket(());

/// 窗口级帧调度（`FrameIndex` 的唯一递增点在这里）。
pub(crate) struct FrameScheduler {
    state: FrameState,
    committed: u32,
}

impl FrameScheduler {
    pub(crate) fn new() -> Self {
        Self {
            state: FrameState::Idle,
            committed: 0,
        }
    }

    /// `Idle → Pending`；返回是否真的发生了转换（幂等）。
    pub(crate) fn request_frame(&mut self) -> bool {
        if self.state == FrameState::Idle {
            self.state = FrameState::Pending;
            true
        } else {
            false
        }
    }

    /// `Pending → Draining`；不在 `Pending` 时返回 `None`（调用方须先 `request_frame`）。
    pub(crate) fn begin(&mut self) -> Option<FrameTicket> {
        if self.state == FrameState::Pending {
            self.state = FrameState::Draining;
            Some(FrameTicket(()))
        } else {
            None
        }
    }

    /// `Draining → Idle`，`committed += 1`，返回新序号（**唯一**递增点）。
    pub(crate) fn commit(&mut self, _ticket: FrameTicket) -> FrameIndex {
        self.state = FrameState::Idle;
        let next = self.committed.checked_add(1);
        debug_assert!(next.is_some(), "FrameIndex overflow (u32::MAX frames)");
        // 溢出时**饱和**而不是回绕到 0：0 是「从未提交」的哨兵，回绕会让 in-flight 判据失效。
        self.committed = next.unwrap_or(u32::MAX);
        let index = FrameIndex(self.committed);
        debug_assert!(index.get() >= 1, "提交序号必须 >= 1");
        index
    }

    /// `Draining → Idle`，序号不变（空闲帧 / 错误中止）。
    pub(crate) fn abort(&mut self, _ticket: FrameTicket) {
        self.state = FrameState::Idle;
    }

    pub(crate) fn committed(&self) -> u32 {
        self.committed
    }
}

/// IME caret hint：`node` 是文本节点，`byte_index` 是该文本内容的 UTF-8 字节偏移。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImeCursorRequest {
    node: NodeId,
    byte_index: usize,
}

/// 一个窗口的全部状态：节点树 + 绘制场景 + DPI + 参数 + 帧调度。
pub struct Window {
    id: WindowId,
    options: WindowOptions,
    tree: NodeTree,
    scene: Scene,
    scale: Scale,
    /// DPI 代际：`set_scale` 递增；`App::refresh_text` 用它判定「图集要换代」（P05）。
    scale_epoch: u32,
    /// 帧调度（`Idle / Pending / Draining`；`FrameIndex` 只在提交时递增）。
    scheduler: FrameScheduler,
    /// 实例编码器（容量跨帧保留；P07 消费 `InstanceBuffer`）。
    encoder: InstanceEncoder,
    /// 本帧的 damage 累加器（`paint_frame` 重置，`commit_frame` 消费）。
    damage: DamageAccumulator,
    /// 颜色解析器（编码期 `resolve`；P14 主题的注入点）。
    resolver: ColorResolver,
    /// GPU 渲染器（`None` = headless，I7 的默认；P09 的窗口层负责 attach 原生窗口）。
    renderer: Option<Renderer>,
    /// 该窗口的渲染器是否已经收到过**完整**的字形图集页（新渲染器 ⇒ `false`）。
    ///
    /// 图集页是进程级共享数据，但纹理是每窗口一份（`AtlasTextures`），因此必须逐窗口记账：
    /// 第一次（或渲染器重建后）必须整页上传，之后才允许按脏行增量（P13 §8.2 / 评审阻塞 ②）。
    glyph_page_received: bool,
    /// 同上，图片图集页。
    image_page_received: bool,
    /// 本帧的分段计时（容量跨帧复用）。
    timings: FrameTimings,
    /// 本帧 lane 绘制序的复用缓冲（避免每帧分配）。
    paint_order: Vec<crate::paint::scene::LaneId>,
    /// 尺寸/DPI 变化 ⇒ 必须整帧重绘（S-64）。
    full_damage_pending: bool,
    /// `App::frame` 在提交前写入的帧统计（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    stats: FrameStats,
    /// 最近一帧的调度/绘制记录（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    record: Option<FrameRecord>,
    /// 焦点（P10 的唯一权威源）。
    focus: FocusScope,
    /// 指针状态（hover / 光标 / 按下 / 拖拽 / 捕获）。
    pointer: PointerState,
    /// 下一帧必须重绘的逻辑矩形（滚动视口；`paint_frame` 消费后清空）。
    pending_damage: Option<Rect>,
    /// a11y 状态机（P11：变更集合 / 全树旗标 / 捕获开关）。
    a11y: crate::a11y::adapter::A11yBridge,
    /// IME caret hint（P16：应用声明的文本节点 + UTF-8 字节偏移）。
    ime_cursor: Option<ImeCursorRequest>,
    /// 最近一帧的实际 GPU 绘制统计（P17 HUD）。
    #[cfg(feature = "hud")]
    render_stats: crate::render::RenderStats,
    /// 最近一帧的 CPU 时间（`begin_frame` → `paint_frame` 结束）。
    #[cfg(feature = "hud")]
    frame_cpu: Option<std::time::Duration>,
    /// 最近两次提交帧入口的间隔（首帧为 `None`）。
    #[cfg(feature = "hud")]
    frame_interval: Option<std::time::Duration>,
    /// 上一次提交帧的入口时间戳（App 时钟；用于 HUD 帧间隔）。
    #[cfg(feature = "hud")]
    last_commit_at: Option<std::time::Instant>,
    /// 当前帧开始时间（仅 CPU 计时用；中止/空闲时清空）。
    #[cfg(feature = "hud")]
    frame_started_at: Option<std::time::Instant>,
}

impl Window {
    /// **唯一的构造点**：`Scene::empty()` 与 `NodeTree::new()` 只在这里组合。
    pub(crate) fn new(id: WindowId, options: WindowOptions) -> Self {
        let scale = options.scale;
        Self {
            id,
            options,
            tree: NodeTree::new(),
            scene: Scene::empty(),
            scale,
            scale_epoch: 0,
            scheduler: FrameScheduler::new(),
            encoder: InstanceEncoder::new(),
            damage: DamageAccumulator::new(scale),
            resolver: ColorResolver::new(Interpolation::default()),
            renderer: None,
            glyph_page_received: false,
            image_page_received: false,
            timings: FrameTimings::new(),
            paint_order: Vec::new(),
            full_damage_pending: false,
            #[cfg(feature = "test-support")]
            stats: FrameStats::default(),
            #[cfg(feature = "test-support")]
            record: None,
            focus: FocusScope::default(),
            pointer: PointerState::new(),
            pending_damage: None,
            a11y: crate::a11y::adapter::A11yBridge::new(),
            ime_cursor: None,
            #[cfg(feature = "hud")]
            render_stats: crate::render::RenderStats::default(),
            #[cfg(feature = "hud")]
            frame_cpu: None,
            #[cfg(feature = "hud")]
            frame_interval: None,
            #[cfg(feature = "hud")]
            last_commit_at: None,
            #[cfg(feature = "hud")]
            frame_started_at: None,
        }
    }

    /// 焦点状态（只读；写入口是 `App::{set_focus, focus_next, focus_prev}`）。
    #[must_use]
    pub fn focus(&self) -> &FocusScope {
        &self.focus
    }

    /// 焦点状态的可写引用（写入口是 `App::set_focus`，它负责标脏）。
    pub(crate) fn focus_mut(&mut self) -> &mut FocusScope {
        &mut self.focus
    }

    /// 指针状态（crate 私有：`App::dispatch_input` 与平台泵是唯一读者）。
    pub(crate) fn pointer(&self) -> &PointerState {
        &self.pointer
    }

    /// 指针状态的可写引用。
    pub(crate) fn pointer_mut(&mut self) -> &mut PointerState {
        &mut self.pointer
    }

    // ── P16：IME caret hint ────────────────────────────────────────────────────

    /// 登记 IME caret hint；返回是否真的变化（同值 no-op）。
    pub(crate) fn set_ime_cursor_request(&mut self, node: NodeId, byte_index: usize) -> bool {
        let next = ImeCursorRequest { node, byte_index };
        if self.ime_cursor == Some(next) {
            return false;
        }
        self.ime_cursor = Some(next);
        true
    }

    /// 清除 IME caret hint；返回是否真的变化。
    pub(crate) fn clear_ime_cursor_request(&mut self) -> bool {
        self.ime_cursor.take().is_some()
    }

    /// 解析当前 IME caret hint（窗口客户区逻辑坐标）。
    ///
    /// `Ok(None)` = 没有 hint；`Err(TextShape)` = 已登记但文本尚未 shape / 文本已变化，
    /// 平台层应跳过本次更新并保留上一次有效区域。
    pub(crate) fn ime_cursor_area(&self) -> Result<Option<Rect>, EngineError> {
        let Some(request) = self.ime_cursor else {
            return Ok(None);
        };
        self.text_caret_rect(request.node, request.byte_index)
            .map(Some)
    }

    /// 文本插入点的窗口绝对矩形（逻辑像素）。
    ///
    /// `NodeTree::text_caret_rect` 给出文本盒局部矩形；本方法补上节点 border box 原点、
    /// 内容盒偏移（border + padding）与祖先滚动偏移。
    ///
    /// # Errors
    /// `StaleNode`：节点不存在；`TextShape`：不是已 shape 的文本节点或 `byte_index` 非法。
    pub fn text_caret_rect(&self, node: NodeId, byte_index: usize) -> Result<Rect, EngineError> {
        let local = self.tree.text_caret_rect(node, byte_index)?;
        let bounds = self.tree.get(node)?.layout().bounds;
        let content_offset = self.tree.content_offset(node);
        let origin = Point::new(
            bounds.origin.x + content_offset.x,
            bounds.origin.y + content_offset.y,
        );
        Ok(scroll::to_screen(&self.tree, node, local.translate(origin)))
    }

    /// 请求下一帧重绘一个逻辑矩形（滚动视口；并集语义）。
    pub(crate) fn request_damage(&mut self, rect: Rect) {
        self.pending_damage = Some(match self.pending_damage {
            Some(existing) => existing.union(rect),
            None => rect,
        });
    }

    /// 窗口句柄。
    #[must_use]
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// 节点树只读视图。
    #[must_use]
    pub fn tree(&self) -> &NodeTree {
        &self.tree
    }

    /// 应用层建树的唯一入口（`NodeBuilder::build` 的 `&mut NodeTree` 来自这里）。
    pub fn tree_mut(&mut self) -> &mut NodeTree {
        &mut self.tree
    }

    /// 只读场景（S-10 / S-33：应用层**拿不到** `&mut Scene`，`Scene` 也**没有**公开构造器）。
    #[must_use]
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// 建窗参数快照。
    #[must_use]
    pub fn options(&self) -> &WindowOptions {
        &self.options
    }

    /// 当前生效的 DPI 比例。
    #[must_use]
    pub fn scale(&self) -> Scale {
        self.scale
    }

    /// 窗口逻辑尺寸（根布局的可用空间）。
    #[must_use]
    pub fn size(&self) -> Size {
        self.options.size
    }

    /// 逻辑尺寸变化（消费者：P12 的 resize 事件）：写尺寸 + 标根 `LAYOUT` + 清根缓存。
    pub fn set_size(&mut self, size: Size) {
        self.options.size = size;
        self.full_damage_pending = true;
        let root = self.tree.root();
        // 与 `set_style` / `set_layout_override` 同一条失效路径（P11：它也标 `A11Y`）。
        self.tree.invalidate_layout(root);
    }

    /// 用窗口逻辑尺寸重排，返回本次布局统计（P06/P09 的帧驱动调用它）。
    ///
    /// **文本节点必须先在本帧调用 `App::refresh_text`**：`NodeBuilder::text` 不设 measure，
    /// 没跑文本 pass 的文本节点测量为 `0×0`（P05 §13.4）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure { node, axis, value }`：某个 measure 闭包返回 NaN / 负数（首条优先）。
    /// 出错时布局盒可能为空，调用方**不得**提交本帧。
    pub fn compute_layout(&mut self) -> Result<LayoutReport, EngineError> {
        let report = self.tree.compute_layout(taffy::geometry::Size {
            width: Some(self.options.size.width),
            height: Some(self.options.size.height),
        })?;
        // P11：布局盒变了的节点必须重发 a11y（bounds = 物理化的 layout 盒）。窗口 resize /
        // 虚拟列表重窗都只标根，子树的新盒由布局 pass 写入，不经过任何 `mark_*`。
        self.tree.mark_moved_a11y();
        Ok(report)
    }

    /// DPI 变化（P12 的事件处理调用它）：更新比例并标根 `LAYOUT | PAINT`。
    ///
    /// **逻辑布局盒与 scale 无关**（I11：布局全用逻辑像素，物理化在 paint/P06），这里只保守地失效。
    ///
    /// **不**置 `App` 的唤醒请求：DPI 事件本身来自平台，平台总是会调度一帧（P12）。
    pub fn set_scale(&mut self, scale: Scale) {
        self.scale = scale;
        self.scale_epoch = self.scale_epoch.wrapping_add(1);
        self.full_damage_pending = true;
        let root = self.tree.root();
        self.tree.invalidate_layout(root);
        // a11y 的 bounds 全是物理像素 ⇒ 比例变了必须整棵重发（P11 §5.5）。
        self.a11y.note_bounds_changed();
    }

    // ── P11：a11y（快照交付 + 捕获） ─────────────────────────────────────────

    /// 本轮是否有 a11y 工作（O(1)；平台泵每轮用它过滤 `update_if_active` 调用）。
    #[must_use]
    pub(crate) fn a11y_pending(&self) -> bool {
        self.a11y.pending(&self.tree)
    }

    /// 产出一笔 a11y 交付（消费 `A11Y|ORDER` 位与 `full_pending`）。
    ///
    /// 调用方：平台层的 `update_if_active` 闭包（生产）、`App::frame` 的捕获路径（测试）。
    pub(crate) fn a11y_take(
        &mut self,
        diagnostics: &mut Vec<crate::a11y::A11yDiagnostic>,
    ) -> Option<crate::a11y::A11yTreeSnapshot> {
        let Self {
            tree,
            a11y,
            focus,
            scale,
            options,
            ..
        } = self;
        a11y.take(
            tree,
            focus.focused(),
            *scale,
            options.title.as_str(),
            diagnostics,
        )
    }

    /// 某个子树内容整体平移（滚动：子孙 bounds 全变 ⇒ 重发整棵子树）。
    pub(crate) fn a11y_note_scroll(&mut self, container: crate::node::id::NodeId) {
        self.a11y.invalidate_subtree(container);
    }

    /// 只有 `bind_a11y` 的节点被写信号（没有像素要画，但下帧仍要跑样式 pass）。
    pub(crate) fn a11y_note_bind_work(&mut self) {
        self.a11y.note_bind_work();
    }

    /// 清掉「a11y 绑定待求值」旗标（`refresh_styles` 的入口处）。
    pub(crate) fn a11y_clear_bind_work(&mut self) {
        self.a11y.clear_bind_work();
    }

    /// 打开/关闭 a11y 快照捕获（仅 `test-support` 的 `TestApp` 调用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, on: bool) {
        self.a11y.set_capture(on);
    }

    /// DPI 代际（`set_scale` 递增；`App::refresh_text` 的图集换代判据）。
    #[must_use]
    pub fn scale_epoch(&self) -> u32 {
        self.scale_epoch
    }

    // ── P06：帧推进 ───────────────────────────────────────────────────────────

    /// 提交游标（0 = 从未提交；空闲帧不变）。
    #[must_use]
    pub fn frame_index(&self) -> FrameIndex {
        FrameIndex(self.scheduler.committed())
    }

    /// 本帧是否需要画面（两段门禁的第一段）。
    ///
    /// `wake` = `App::take_wake_request()` 的结果（`App::frame` 传入；`Window::frame` 传 `false`）。
    /// `!REUSABLE` 表示 lane 内容与树/样式不一致；`LAYOUT | PAINT` 覆盖「新建/未标 REUSABLE」的节点。
    pub(crate) fn needs_frame(&self, wake: bool) -> bool {
        // `a11y_bind_pending`：只有 `bind_a11y` 的节点被写信号时**没有**像素要画，但
        // `refresh_styles` 仍必须跑一次（否则补丁永远不更新）。帧末的门禁会把它退回 `Idle`。
        wake || self.full_damage_pending
            || self.pending_damage.is_some()
            || self.a11y.bind_work()
            || self.tree.any_node(|node| {
                let dirty = node.dirty();
                dirty.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT)
                    || !dirty.contains(DirtyFlags::REUSABLE)
            })
    }

    /// `Idle → Pending → Draining`；空闲/重复调用返回 `None`。
    pub(crate) fn begin_frame(&mut self) -> Option<FrameTicket> {
        if !self.scheduler.request_frame() {
            // 已经是 Pending/Draining：重复请求不产生新帧。
            return None;
        }
        let ticket = self.scheduler.begin()?;
        #[cfg(feature = "hud")]
        {
            self.frame_started_at = Some(std::time::Instant::now());
        }
        Some(ticket)
    }

    /// 仅当有 `LAYOUT` 时才重排（纯颜色变化不跑布局）。
    pub(crate) fn layout_if_dirty(&mut self) -> Result<Option<LayoutReport>, EngineError> {
        if !self
            .tree
            .any_node(|node| node.dirty().contains(DirtyFlags::LAYOUT))
        {
            return Ok(None);
        }
        self.compute_layout().map(Some)
    }

    /// 最近一帧的编码结果（渲染层上传源；P08 的 `Renderer` 消费）。
    #[must_use]
    pub(crate) fn instance_buffer(&self) -> &InstanceBuffer {
        self.encoder.buffer()
    }

    /// 本帧编码出的实例字节（P07 的上传入口；应用层不得依赖其内容）。
    ///
    /// 返回最近一帧 `InstanceEncoder` 的 `&[u8]`（`GpuPrimitive` 数组的字节视图，P02 §8.1 的 128 B 布局）。
    #[must_use]
    pub fn instance_bytes(&self) -> &[u8] {
        self.instance_buffer().as_bytes()
    }

    /// 是否已有 GPU 渲染器（`App::upload_atlas_pages` 的判据）。
    pub(crate) fn has_renderer(&self) -> bool {
        self.renderer.is_some()
    }

    /// 该窗口的渲染器是否已收到完整字形图集页。
    pub(crate) fn glyph_page_received(&self) -> bool {
        self.glyph_page_received
    }

    /// 该窗口的渲染器是否已收到完整图片图集页。
    pub(crate) fn image_page_received(&self) -> bool {
        self.image_page_received
    }

    /// 记账：字形页已整页上传到本窗口的渲染器。
    pub(crate) fn mark_glyph_page_received(&mut self) {
        self.glyph_page_received = true;
    }

    /// 记账：图片页已整页上传到本窗口的渲染器。
    pub(crate) fn mark_image_page_received(&mut self) {
        self.image_page_received = true;
    }

    /// 把一页图集像素交给渲染器（下一次 `submit` 上传；无渲染器时丢弃）。
    ///
    /// 消费者：`App::upload_atlas_pages`（P13：**广播**给所有窗口的渲染器；
    /// 图集页是进程级共享数据，纹理是每窗口一份）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: crate::paint::atlas::AtlasKind,
        pixels: &[u8],
        rows: crate::render::DirtyRows,
    ) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.queue_atlas_upload(kind, pixels, rows);
        }
    }

    /// 挂上 GPU 渲染器（平台层在窗口创建之后调用；`Renderer` 已带 HWND swapchain）。
    ///
    /// 换渲染器 = 换一套空纹理（`AtlasTextures`），所以两张图集页都要重新整页上传。
    pub(crate) fn set_renderer(&mut self, renderer: Renderer) {
        self.renderer = Some(renderer);
        self.glyph_page_received = false;
        self.image_page_received = false;
    }

    /// 轮询 render 线程的完成/错误反馈（macOS 窗口路径；空闲时也要跑）。
    #[cfg(target_os = "macos")]
    pub(crate) fn poll_renderer(&mut self) -> Result<(), EngineError> {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.poll()?;
        }
        Ok(())
    }

    /// 启用**离屏** GPU 渲染（headless 行为不变：不调用就恒为 `None`）。
    ///
    /// P09 的原生窗口层会在此之后 attach swapchain；本方法本身只把窗口的绘制目标变成
    /// 持久 offscreen（快照 / 导出 / 无窗口验证），从不 present。
    ///
    /// # Errors
    /// `EngineError::UnsupportedGpu`（D8 基线不足）、`EngineError::AppInit`（设备 / 管线创建失败）、
    /// `EngineError::ShaderLoad`（DXIL 产物缺失或被跳过）。
    pub fn enable_offscreen_renderer(&mut self) -> Result<(), EngineError> {
        if self.renderer.is_some() {
            return Ok(());
        }
        let surface = self.physical_surface();
        self.renderer = Some(Renderer::new_offscreen(surface)?);
        self.glyph_page_received = false;
        self.image_page_received = false;
        Ok(())
    }

    /// 窗口的物理像素尺寸（`size × scale` 的 covering 结果）。
    fn physical_surface(&self) -> (u32, u32) {
        let rect = Rect::new(0.0, 0.0, self.size().width, self.size().height)
            .to_physical_covering(self.scale);
        (rect.width().max(1) as u32, rect.height().max(1) as u32)
    }

    /// 是否有节点需要重建 lane（`LAYOUT` / `PAINT` / `REUSABLE` 不在）。
    ///
    /// `A11Y` 不在判据里：a11y 更新不产生 damage（`design §7.5` / P03 §9.3 表第 9 行）。
    fn needs_node_repaint(&self) -> bool {
        self.tree.any_node(|node| {
            let dirty = node.dirty();
            dirty.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT)
                || !dirty.contains(DirtyFlags::REUSABLE)
        })
    }

    /// lane 重建/复用 + damage + 实例编码（`design/phase-06-paint-runtime.md` §9.6）。
    ///
    /// # Errors
    /// [`EngineError::ClipDepthExceeded`]：clip 嵌套超过上限（release 也失败，见 P14 审查）。
    /// 返回 `Err` 时本帧未提交：调用方必须 `abort_frame`，damage/dirty 保留待重试。
    pub(crate) fn paint_frame(&mut self) -> Result<PaintReport, EngineError> {
        // 借用式取出（保容量）：`tree` 与 `scene` 是不同字段，循环内可同时可变借用。
        for lane in self.tree.drain_removed_lanes() {
            self.scene.release_lane(lane);
        }
        let scale = self.scale;
        let size = self.size();
        let frame = Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
        self.damage.reset(scale);
        let has_damage = self.full_damage_pending || self.pending_damage.is_some();
        if self.full_damage_pending {
            self.damage.push_full();
            self.full_damage_pending = false;
        }
        // 滚动（P10 §7.8）：唯一来源是视口矩形；节点级 damage 不参与（偏移不写 `layout.bounds`）。
        if let Some(pending) = self.pending_damage.take() {
            self.damage.push(pending, 1.0, None);
        }
        // P11：只有 `A11Y` 脏（`bind_a11y` 重算）的帧**没有任何像素要画**：早退让 lane 保持
        // `REUSABLE`、damage 保持空，帧末门禁把本帧退回 `Idle`（P11 审查 #6）。
        if !has_damage && !self.needs_node_repaint() {
            return Ok(PaintReport {
                rebuilt: 0,
                damage: crate::render::damage::DamageSummary::default(),
                #[cfg(feature = "test-support")]
                instances: 0,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            });
        }
        self.paint_order.clear();
        let focused = self.focus.focused();
        let mut pass = PaintPass {
            tree: &mut self.tree,
            scene: &mut self.scene,
            damage: &mut self.damage,
            scale,
            frame,
            order: &mut self.paint_order,
            focused,
            #[cfg(feature = "test-support")]
            log: PaintLog::default(),
        };
        let mut report = pass.run()?;
        let buffer = self.encoder.encode(&self.scene, scale, &self.resolver);
        #[cfg(feature = "test-support")]
        {
            report.instances = buffer.len();
        }
        #[cfg(not(feature = "test-support"))]
        {
            let _ = buffer;
            let _ = &mut report;
        }
        #[cfg(feature = "hud")]
        {
            self.frame_cpu = self.frame_started_at.map(|started| started.elapsed());
        }
        Ok(report)
    }

    /// 提交（或在中止条件下退回 `Idle`）。
    ///
    /// 第二段门禁：`rebuilt == 0` 且没有任何 damage ⇒ 没有像素要画 ⇒ 不提交、`FrameIndex` 不变。
    ///
    /// # Errors
    /// 挂了渲染器时，GPU 提交失败（device lost / present 失败 / swapchain 重建失败）会原样返回；
    /// 此时 `FrameIndex` 已递增、damage 已结算，调用方**不得**重试本帧。
    pub(crate) fn commit_frame(
        &mut self,
        ticket: FrameTicket,
        report: PaintReport,
    ) -> Result<FrameOutcome, EngineError> {
        let has_pixels = report.rebuilt > 0 || report.damage.full || report.damage.rects > 0;
        // 默认构建也算的读点：摘要计数必须与累加器的实际矩形数一致（P07 消费同一累加器）。
        debug_assert_eq!(
            report.damage.rects,
            self.damage.rects().len(),
            "damage summary and accumulator disagree"
        );
        if !has_pixels {
            self.scheduler.abort(ticket);
            #[cfg(feature = "hud")]
            {
                self.frame_started_at = None;
                self.frame_cpu = None;
            }
            #[cfg(feature = "test-support")]
            self.record_idle();
            return Ok(FrameOutcome::Idle);
        }
        let index = self.scheduler.commit(ticket);
        self.tree.settle_frame();
        if let Some(mut renderer) = self.renderer.take() {
            let result = self.submit_gpu(&mut renderer, index, report.damage);
            self.renderer = Some(renderer);
            let stats = result?;
            #[cfg(feature = "hud")]
            {
                self.render_stats = stats;
            }
            #[cfg(not(feature = "hud"))]
            {
                let _ = stats;
            }
        }
        #[cfg(feature = "test-support")]
        {
            let frame_rect = Rect::new(0.0, 0.0, self.size().width, self.size().height)
                .to_physical_covering(self.scale);
            let stats = std::mem::take(&mut self.stats);
            let record = crate::test_support::record::capture(
                &self.scene,
                &self.tree,
                &self.resolver,
                &report,
                self.damage.rects(),
                frame_rect,
                index,
                stats,
            );
            self.record = Some(record);
        }
        Ok(FrameOutcome::Committed(index))
    }

    /// 把本帧交给 GPU（headless 不进入；`renderer` 由调用方临时取出以避免借用冲突）。
    fn submit_gpu(
        &mut self,
        renderer: &mut Renderer,
        index: FrameIndex,
        damage: DamageSummary,
    ) -> Result<crate::render::RenderStats, EngineError> {
        let surface = self.physical_surface();
        let frame = Rect::new(0.0, 0.0, self.size().width, self.size().height)
            .to_physical_covering(self.scale);
        #[cfg(target_os = "macos")]
        renderer.set_scale(f64::from(self.scale.factor()));
        let fresh = renderer.surface() != surface || renderer.is_fresh();
        if fresh {
            renderer.resize(surface)?;
        }
        let plan = plan(damage, self.damage.rects(), frame);
        if plan == Plan::Idle && !fresh {
            return Ok(crate::render::RenderStats::default());
        }
        let mut frame_plan = FramePlan::new(plan, fresh);
        if frame_plan.is_idle() {
            return Ok(crate::render::RenderStats::default());
        }
        let Window {
            encoder,
            timings,
            damage,
            ..
        } = self;
        let _ = damage;
        renderer.submit(index, encoder.buffer(), &mut frame_plan, timings)
    }

    /// 中止（错误路径）：不结算、不递增序号（damage/dirty 保留，下一次重试）。
    pub(crate) fn abort_frame(&mut self, ticket: FrameTicket) {
        self.scheduler.abort(ticket);
        #[cfg(feature = "hud")]
        {
            self.frame_started_at = None;
            self.frame_cpu = None;
        }
        #[cfg(feature = "test-support")]
        self.record_idle();
    }

    /// 窗口本地帧（**无 App 通道**：不跑样式/文本 pass）。
    ///
    /// 含 `bind_style` / `bind_text` 的窗口必须走 `App::frame`，否则补丁不会重新求值
    /// （debug 构建会直接断言失败）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure`：布局闭包返回非有限值/负尺寸（本帧已中止，未提交）。
    pub fn frame(&mut self) -> Result<FrameOutcome, EngineError> {
        if !self.needs_frame(false) {
            #[cfg(feature = "test-support")]
            self.record_idle();
            return Ok(FrameOutcome::Idle);
        }
        debug_assert!(
            !self.tree.any_node(crate::node::tree::Node::has_bindings),
            "Window::frame does not run style/text passes; use App::frame for bound nodes"
        );
        let Some(ticket) = self.begin_frame() else {
            return Ok(FrameOutcome::Idle);
        };
        match self.layout_if_dirty() {
            Ok(_) => {}
            Err(error) => {
                self.abort_frame(ticket);
                return Err(error);
            }
        }
        let report = match self.paint_frame() {
            Ok(report) => report,
            Err(error) => {
                self.abort_frame(ticket);
                return Err(error);
            }
        };
        self.commit_frame(ticket, report)
    }

    /// 最近一帧的调度记录（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    pub(crate) fn record(&self) -> Option<&FrameRecord> {
        self.record.as_ref()
    }

    /// 记录一次空闲帧（不提交、序号不变）。
    #[cfg(feature = "test-support")]
    pub(crate) fn record_idle(&mut self) {
        self.record = Some(FrameRecord::idle(
            FrameIndex(self.scheduler.committed()),
            std::mem::take(&mut self.stats),
        ));
    }

    /// 把一笔 a11y 交付挂到**最近一帧**的记录上（`TestApp::drain` 在帧之后调用，与平台泵同序）。
    #[cfg(feature = "test-support")]
    pub(crate) fn attach_a11y(&mut self, snapshot: crate::a11y::A11yTreeSnapshot) {
        if let Some(record) = self.record.as_mut() {
            record.a11y = Some(snapshot);
        }
    }

    /// `App::frame` 在提交前写入本帧的统计（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_frame_stats(&mut self, stats: FrameStats) {
        self.stats = stats;
    }

    /// 重置 HUD 的提交时间基准（开启/重新开启 HUD 时调用）。
    #[cfg(feature = "hud")]
    pub(crate) fn reset_debug_hud_timing(&mut self) {
        self.render_stats = crate::render::RenderStats::default();
        self.frame_cpu = None;
        self.frame_interval = None;
        self.last_commit_at = None;
        self.frame_started_at = None;
    }

    /// 记录一次提交的时间戳（P17 HUD 的帧间隔/FPS 来源）。
    #[cfg(feature = "hud")]
    pub(crate) fn note_debug_hud_commit(&mut self, now: std::time::Instant) {
        self.frame_interval = self
            .last_commit_at
            .map(|previous| now.saturating_duration_since(previous));
        self.last_commit_at = Some(now);
    }

    /// 最近提交帧的 HUD 指标（P17）。
    #[cfg(feature = "hud")]
    pub(crate) fn debug_hud_metrics(&self) -> crate::debug_hud::DebugHudMetrics {
        use crate::render::timer::Section;

        let (physical_width, physical_height) = self.physical_surface();
        let mut upload_time = self
            .timings
            .sample(Section::Upload)
            .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns));
        let mut gpu_time = self
            .timings
            .sample(Section::Gpu)
            .map(|sample| std::time::Duration::from_nanos(sample.gpu_ns));
        let mut present_time = self
            .timings
            .sample(Section::Present)
            .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns));
        // macOS 窗口路径的 GPU 工作已移到 render 线程：主线程没有本帧的 samples，
        // 用最近完成帧的 worker 采样补齐（HUD 允许领先一帧）。
        #[cfg(target_os = "macos")]
        if let Some(renderer) = self.renderer.as_ref() {
            upload_time = upload_time.or_else(|| {
                renderer
                    .timing_sample(Section::Upload)
                    .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns))
            });
            gpu_time = gpu_time.or_else(|| {
                renderer
                    .timing_sample(Section::Gpu)
                    .map(|sample| std::time::Duration::from_nanos(sample.gpu_ns))
            });
            present_time = present_time.or_else(|| {
                renderer
                    .timing_sample(Section::Present)
                    .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns))
            });
        }
        crate::debug_hud::DebugHudMetrics {
            frame_index: self.frame_index(),
            frame_interval: self.frame_interval,
            cpu_time: self.frame_cpu,
            upload_time,
            gpu_time,
            present_time,
            draw_calls: self.render_stats.draw_calls,
            instances: self.render_stats.instances,
            primitives: self.scene.primitive_count(),
            physical_width,
            physical_height,
            scale: self.scale.factor(),
        }
    }

    /// 强制全量重绘（oracle 的「全量」路径）：清 `REUSABLE` + 清空所有 lane 的内容。
    #[cfg(feature = "test-support")]
    pub(crate) fn force_full_repaint(&mut self) {
        self.tree.clear_all_reusable();
        self.scene.clear_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameIndex, FrameOutcome, Window, WindowId, WindowOptions};
    use crate::{
        math::{Scale, Size},
        node::dirty::DirtyFlags,
    };

    fn window() -> Window {
        Window::new(WindowId::for_test(1), WindowOptions::default())
    }

    #[test]
    fn new_window_has_root_and_empty_scene() {
        let window = window();

        assert_eq!(window.id(), WindowId::for_test(1));
        assert_eq!(window.tree().order().len(), 1, "初始只有 root");
        assert_eq!(window.scene().lane_count(), 0);
        assert!(window.scene().is_empty());
        assert_eq!(window.scale(), Scale::ONE);
    }

    #[test]
    fn scene_starts_without_lanes() {
        let window = window();
        let scene = window.scene();

        assert_eq!(scene.lane_count(), 0);
        assert!(scene.order().is_empty());
        assert_eq!(scene.primitive_count(), 0);
        assert!(scene.is_empty());
    }

    #[test]
    fn set_scale_marks_root_layout() {
        let mut window = window();
        window.tree_mut().clear_all_dirty_for_test();
        let scale = Scale::new(2.0).expect("合法比例");

        window.set_scale(scale);

        assert_eq!(window.scale(), scale);
        let root = window.tree().root();
        let flags = window.tree().get(root).expect("root 存在").dirty();
        assert!(flags.contains(DirtyFlags::LAYOUT));
        assert!(flags.contains(DirtyFlags::PAINT));
    }

    #[test]
    fn window_options_default_values() {
        let options = WindowOptions::default();

        assert_eq!(options.title, "");
        assert_eq!(options.size.width, 800.0);
        assert_eq!(options.size.height, 600.0);
        assert_eq!(options.scale, Scale::ONE);
    }

    #[test]
    fn window_size_is_root_available_space() {
        use crate::node::builder::NodeBuilder;

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(120.0, 60.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let child = NodeBuilder::boxed()
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let report = window.compute_layout().expect("布局成功");

        assert_eq!(window.size(), Size::new(120.0, 60.0));
        assert_eq!(
            window.tree().get(root).expect("root").layout().bounds.size,
            Size::new(120.0, 60.0),
            "根 100% × 100% 撑满窗口"
        );
        assert!(report.laid_out >= 2, "根与子节点都要布局：{report:?}");
        let _ = child;
    }

    #[test]
    fn dpi_scale_does_not_change_logical_boxes() {
        use crate::{
            math::Rect,
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
        };

        fn fixed(width: f32, height: f32) -> Style {
            let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
            inner.size = taffy::geometry::Size {
                width: taffy::style::Dimension::length(width),
                height: taffy::style::Dimension::length(height),
            };
            Style {
                inner,
                clip: Clip::None,
            }
        }

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(120.0, 60.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let child = NodeBuilder::boxed()
            .style(fixed(10.5, 3.25))
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");
        let before = window.tree().get(child).expect("存在").layout().bounds;

        window.set_scale(Scale::new(1.5).expect("合法比例"));
        let _ = window.compute_layout().expect("布局成功");

        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds,
            before,
            "逻辑布局盒与 scale 无关（物理化在 P06）"
        );
        assert_eq!(before, Rect::new(0.0, 0.0, 10.5, 3.25));
    }

    #[test]
    fn resize_relayouts_percent_children() {
        use crate::{
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
        };

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(100.0, 100.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::percent(1.0),
            height: taffy::style::Dimension::percent(1.0),
        };
        let child = NodeBuilder::boxed()
            .style(Style {
                inner,
                clip: Clip::None,
            })
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");
        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds.size,
            Size::new(100.0, 100.0)
        );

        window.set_size(Size::new(300.0, 50.0));
        let _ = window.compute_layout().expect("布局成功");

        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds.size,
            Size::new(300.0, 50.0),
            "百分比尺寸随根可用空间变化"
        );
    }

    #[test]
    fn set_size_marks_root_layout_and_clears_cache() {
        use crate::node::builder::NodeBuilder;

        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let _ = NodeBuilder::boxed()
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");

        window.set_size(Size::new(640.0, 480.0));

        let flags = window.tree().get(root).expect("root 存在").dirty();
        assert!(
            flags.contains(DirtyFlags::LAYOUT),
            "尺寸变化必须标根 LAYOUT（实际 {flags:?}）"
        );
        assert_eq!(window.size(), Size::new(640.0, 480.0));
    }
    #[test]
    fn scale_epoch_bumps_on_dpi_change() {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        assert_eq!(window.scale_epoch(), 0);

        window.set_scale(Scale::new(1.5).expect("finite scale"));
        assert_eq!(window.scale_epoch(), 1);

        window.set_scale(Scale::new(2.0).expect("finite scale"));
        assert_eq!(window.scale_epoch(), 2);
        assert_eq!(window.scale().factor(), 2.0);
    }

    // ── P06：帧推进 ──

    use crate::{
        node::builder::NodeBuilder,
        paint::{
            color::Color,
            style::{PaintSpec, StylePatch},
        },
    };

    fn painted_window() -> (Window, crate::node::id::NodeId) {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 40.0, 16.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            })
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        (window, node)
    }

    #[test]
    fn frame_commits_once_and_increments_index() {
        let (mut window, _node) = painted_window();
        assert_eq!(window.frame_index().get(), 0);

        let outcome = window.frame().expect("帧成功");
        assert_eq!(outcome, FrameOutcome::Committed(FrameIndex(1)));
        assert_eq!(window.frame_index().get(), 1);
    }

    #[test]
    fn idle_frame_does_not_commit_or_increment() {
        let (mut window, _node) = painted_window();
        let _ = window.frame().expect("首帧");
        assert_eq!(window.frame_index().get(), 1);

        let outcome = window.frame().expect("第二帧");
        assert_eq!(outcome, FrameOutcome::Idle);
        assert_eq!(window.frame_index().get(), 1, "空闲帧不得递增序号");
    }

    #[test]
    fn paint_only_frame_skips_layout_and_reuses_lanes() {
        let (mut window, node) = painted_window();
        let _ = window.frame().expect("首帧");

        let patch = StylePatch {
            background: Some(Color::BLACK),
            ..StylePatch::default()
        };
        assert!(window.tree_mut().set_style_patch(node, patch));
        let _ = window.tree_mut().mark_paint(node);

        assert!(
            window.layout_if_dirty().expect("无错误").is_none(),
            "只改颜色不应触发布局"
        );
        let outcome = window.frame().expect("第二帧");
        assert!(matches!(outcome, FrameOutcome::Committed(_)));
    }

    #[test]
    fn frame_aborts_on_layout_error() {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let _ = NodeBuilder::boxed()
            .measure(Box::new(
                |_: taffy::Size<Option<f32>>, _: taffy::Size<taffy::AvailableSpace>| {
                    Size::new(f32::NAN, 10.0)
                },
            ))
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let outcome = window.frame();
        assert!(matches!(
            outcome,
            Err(crate::error::EngineError::InvalidMeasure { .. })
        ));
        assert_eq!(window.frame_index().get(), 0, "中止不得递增序号");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "use App::frame")]
    fn frame_rejects_bound_nodes_without_styles() {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
            .bind_style(|_cx| StylePatch::default())
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let _ = window.frame();
    }
}

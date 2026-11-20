//! paint pass：把节点树 + 样式 + 文本 + 自绘图元变成 `Scene` 的 lane 与 damage。
//!
//! 契约：
//! - **一节点一 lane**（`Node.lane`，粘性保留）；
//! - lane 内桶序 = 祖先 clip 前缀 → Shadow → 背景/渐变 → 边框 → 内容 → 祖先 clip 后缀；
//! - 复用判据 = `REUSABLE` + lane 存在 + **clip 上下文一致**（`clip_key`，审查 B1 的守卫）；
//! - 隐藏子树（自身/祖先 `display: none`）清空 lane、不产生像素、`clip_key` 置空（重新显示必重建）；
//! - damage 按「节点级 + 图元级」双写（§6.6）。

use std::{rc::Rc, sync::Arc};

use smallvec::SmallVec;

#[cfg(feature = "test-support")]
use crate::test_support::record::{LaneOutcome, LanePaintEntry, PaintLog};
use crate::{
    error::EngineError,
    math::{PhysicalRect, Point, Rect, Scale},
    node::{
        id::NodeId,
        tree::{BakedGeometryChunk, NodeKind, NodeTree, PaintNodeState},
    },
    paint::{
        color::Color,
        primitive::{
            ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, PositionedGlyph, Primitive, Quad,
            QuadPaint, Shadow,
        },
        scene::{LaneId, Scene},
        style::{Clip, PaintSpec, StylePatch},
    },
    render::{
        AtlasAllocator,
        damage::{DamageAccumulator, DamageSummary},
    },
    text::{ShapedLine, ShapedTextItem},
};

/// 一帧的 lane 重建/复用。字段是 crate 内可见（`pub(crate)` 结构的 `pub` 字段不触发 lint）。
pub(crate) struct PaintPass<'a> {
    pub tree: &'a mut NodeTree,
    /// 静态几何注册表：几何内容的 damage 用它自己的 AABB（`GeometryDesc.bounds`），
    /// 而不是节点布局盒 —— 顶点可以在节点盒之外（`desc.bounds` 才是真实可见范围）。
    pub geometry: &'a crate::paint::geometry_store::GeometryStore,
    pub scene: &'a mut Scene,
    pub damage: &'a mut DamageAccumulator,
    pub scale: Scale,
    /// 本帧 framebuffer 的物理矩形（`DamageAccumulator::finish` 的面积阈值用）。
    pub frame: PhysicalRect,
    /// 本帧的 lane 绘制序（复用调用方的容量）。
    pub order: &'a mut Vec<LaneId>,
    /// 本帧的焦点节点（画 `Style.focus_ring`；`None` = 无焦点）。
    pub focused: Option<NodeId>,
    /// 本帧的字形图集（`Some` = 绘制前硬校验 + 编码期逐 tile 存活校验；`None` = 无 App 的
    /// headless/测试路径跳过）。
    ///
    /// D-5：`ShapedLine` 里的 `AtlasTile`/UV 是生成时的快照。若图集已换代而该行未被重排，
    /// 继续编码就是画旧 UV（整窗花字）。这里把「行代际 == 当前代际」变成绘制前硬校验：
    /// 不匹配的行**本帧不编码**，节点收进 [`PaintReport::stale_text`]，由 `App::frame` 在提交后
    /// 重新入队（下一帧补齐）。
    ///
    /// E1d（F15 方案①）：Canvas 内已经写入的 tile 用 [`AtlasAllocator::is_alive`] 逐 tile 校验
    /// （代际 + 槽位分配身份），把「逐 tile 淘汰不换代」这类失效也拦在编码前。
    pub atlas: Option<&'a AtlasAllocator>,
    /// 本帧的**滚动槽表**（槽位 → （逻辑滚动位移，物理视口））。
    ///
    /// 全量路径每帧重写它：下标 0 恒是「不随滚动偏移」的哨兵（偏移 0、零面积视口），
    /// 滚动容器按绘制序依次占 1..。槽位编号在树结构不变时**逐帧稳定**，因此滚动容器
    /// 的 lane 不需要因为滚动而重建（`Lane::scroll` 参与复用判据）。
    ///
    /// 增量路径**不重写**（结构未变 ⇒ 编号稳定）：只把被访问滚动容器的偏移 / 视口写回
    /// 缓存槽位（`NodeTree::scroll_slot_cached`），必要时追加新槽。
    pub scroll_slots: &'a mut Vec<(Point, PhysicalRect)>,
    /// 与 `scroll_slots` 平行的「槽位 → 容器节点」（下标 i ⇒ 槽位 i + 1）。消费者：纯滚动帧
    /// 的快路径（`window/paint.rs`）据它把偏移刷进槽位表，不必重新遍历树。
    pub scroll_slot_nodes: &'a mut Vec<NodeId>,
    /// lane 重建/复用记录（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
}

/// paint 遍历的两种路径（A1：脏子树剪枝的「双路径」）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaintMode {
    /// 全量 DFS：访问每个节点、重建绘制序、重建滚动槽表（首帧 / 结构变化 / 全帧 damage /
    /// 脏节点状态无法增量处理时的兜底）。
    Full,
    /// 增量 DFS：只访问 `mark_paint_closure` 的脏闭包（非 reusable 节点会保守展开整棵子树，
    /// 以覆盖 clip / 隐藏上下文的传播），不 push 绘制序，滚动槽按缓存编号写回。
    Incremental,
}

impl PaintMode {
    const fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// 一次 paint 的产物。
pub(crate) struct PaintReport {
    pub rebuilt: usize,
    pub damage: DamageSummary,
    /// D-5：因「行代际 ≠ 当前图集代际」而**未编码**的文本节点（本帧不画旧 UV）。
    ///
    /// 消费者：`App::frame` 在 `commit_frame` 之后逐节点 `mark_text_layout_dirty + mark_paint`
    /// 并请求下一帧（下一帧的 `layout_text` 用当前代际重光栅）。
    pub stale_text: SmallVec<[NodeId; 4]>,
    /// D-5 的 canvas 半边（v2.1 E1b + E1d）：因含**已失效 `AtlasTile`**（整页换代，或逐 tile
    /// LRU 淘汰后 tile 已被复用）而**未编码**的 `GlyphRun` 数（Canvas 内文本）。
    ///
    /// Canvas 内容归应用所有，引擎无法自动重建：这里只止损（不画旧 UV，少字优于花字）并计数。
    /// 应用义务：发现句柄 `is_stale()` 后重新 `shape_text` 并用新句柄重建 Canvas。消费者：
    /// `App::frame`（压 [`crate::text::TextDiagnostic::CanvasTextStale`]）与
    /// `test_support::record::FrameRecord::stale_canvas_runs`（帧记录）。
    pub stale_canvas_runs: usize,
    /// 本帧编码出的实例数（仅 `test-support`；`Window::paint_frame` 编码后回填）。
    #[cfg(feature = "test-support")]
    pub instances: usize,
    /// 本帧**重编码**的 lane 数（增量实例编码的断言面；`0` = 全部段都复用）。
    #[cfg(feature = "test-support")]
    pub encoded_lanes: usize,
    /// 本帧整段复用的 lane 段数。
    #[cfg(feature = "test-support")]
    pub reused_segments: usize,
    /// 本帧重编码写入实例环的字节数（增量上传量的编码器侧值）。
    #[cfg(feature = "test-support")]
    pub uploaded_bytes: usize,
    /// 本帧重编码的几何块 op 数（X5：K 线追加应恒为 1）。
    #[cfg(feature = "test-support")]
    pub geometry_encoded_chunks: usize,
    /// 本帧 GPU 侧实际上传的几何字节数（X5；渲染器路径回填，headless 恒 0）。
    #[cfg(feature = "test-support")]
    pub geometry_uploaded_bytes: usize,
    /// 本帧滚动偏移变化的槽数（`FrameStats::scroll_offset_frames` 的 inline 来源）。
    #[cfg(feature = "test-support")]
    pub scroll_slots: usize,
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
    /// 本帧 `SceneDelta` 的结构快照（`Window::paint_frame` 在构建 delta 后回填）。
    #[cfg(feature = "test-support")]
    pub delta: crate::test_support::record::DeltaRecord,
}

impl PaintPass<'_> {
    /// 执行整帧 paint（**全量路径**；唯一入口的消费者：`Window::paint_frame` 的帧分类）。
    ///
    /// # Errors
    /// [`EngineError::ClipDepthExceeded`]：clip 嵌套超过 `MAX_CLIP_DEPTH`。release 也必须失败——
    /// 静默丢弃最外层 clip 会让子内容越界可见（P06 二轮 / P14 审查）。
    pub(crate) fn run(&mut self) -> Result<PaintReport, EngineError> {
        self.execute(PaintMode::Full)
    }

    /// 执行**增量路径**：只 DFS `mark_paint_closure` 的脏闭包（调用方已标记完成）。
    ///
    /// 与全量路径共用同一份节点状态逻辑（隐藏 / clip / 复用 / 重建 / 滚动槽），差异只有两点：
    /// 不 push 绘制序（顺序不变时 `Scene::order` 整体保留，`delta.order` 因此是 `None`），
    /// 以及新建 / 释放 lane 时用轻量 DFS 兜底重建绘制序。
    ///
    /// # Errors
    /// 同 [`PaintPass::run`]。
    pub(crate) fn run_incremental(&mut self) -> Result<PaintReport, EngineError> {
        self.execute(PaintMode::Incremental)
    }

    fn execute(&mut self, mode: PaintMode) -> Result<PaintReport, EngineError> {
        let tree = &mut *self.tree;
        let scene = &mut *self.scene;
        let damage = &mut *self.damage;
        let order = &mut *self.order;
        let scroll_slots = &mut *self.scroll_slots;
        let scroll_slot_nodes = &mut *self.scroll_slot_nodes;
        let scale = self.scale;
        let frame = self.frame;
        #[cfg(feature = "test-support")]
        let log = std::mem::take(&mut self.log);
        if mode.is_full() {
            // 全量路径从零重建绘制序与滚动槽表；增量路径保留上一帧的两者（顺序 / 编号稳定）。
            order.clear();
            tree.begin_full_scroll_slots();
            // 槽位 0 是「不随滚动偏移」的哨兵：`FrameOp::Instances.scroll = 0` 的 op 永远拿到
            // 零偏移，因此不需要在绘制期判断「这个 op 是否在滚动容器里」。
            scroll_slots.clear();
            scroll_slots.push((Point::ZERO, PhysicalRect::new(0, 0, 0, 0)));
            scroll_slot_nodes.clear();
        }
        let mut context = PaintContext {
            geometry: self.geometry,
            tree,
            scene,
            damage,
            scale,
            mode,
            order,
            scroll_slots,
            scroll_slot_nodes,
            focused: self.focused,
            atlas: self.atlas,
            stale_text: SmallVec::new(),
            stale_canvas_runs: 0,
            #[cfg(feature = "test-support")]
            log,
            rebuilt: 0,
            visited: 0,
            order_changed: false,
            baked_scratch: SmallVec::new(),
            rebuild_scratch: SmallVec::new(),
        };
        let mut clip = ClipContext::default();
        let root = context.tree.root();
        // 全量路径的根恒访问；增量路径的根只有在闭包里（= 有脏节点）时才访问。
        paint_node(
            &mut context,
            root,
            false,
            &mut clip,
            Point::ZERO,
            Point::ZERO,
            0,
            0,
            mode.is_full(),
        )?;
        match context.mode {
            PaintMode::Full => context.scene.set_order(context.order),
            PaintMode::Incremental if context.order_changed => {
                // 绘制序成员变了（新建 / 释放 lane）：用轻量 DFS（只读 children + lane_of，
                // 无 clip/damage 计算）重建，再让 `Scene::set_order` 与上一帧比较。
                rebuild_order_only(context.tree, context.order);
                context.scene.set_order(context.order);
                context.tree.note_order_rebuild();
            }
            PaintMode::Incremental => {}
        }
        let damage = context.damage.finish(frame);
        context
            .tree
            .note_paint_frame(context.mode.is_full(), context.visited);
        Ok(PaintReport {
            rebuilt: context.rebuilt,
            damage,
            stale_text: context.stale_text,
            stale_canvas_runs: context.stale_canvas_runs,
            #[cfg(feature = "test-support")]
            instances: 0,
            #[cfg(feature = "test-support")]
            encoded_lanes: 0,
            #[cfg(feature = "test-support")]
            reused_segments: 0,
            #[cfg(feature = "test-support")]
            uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            geometry_encoded_chunks: 0,
            #[cfg(feature = "test-support")]
            geometry_uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            scroll_slots: 0,
            #[cfg(feature = "test-support")]
            log: context.log,
            #[cfg(feature = "test-support")]
            delta: crate::test_support::record::DeltaRecord::default(),
        })
    }
}

/// 一次遍历的借用包（避免 `&mut self` 与 `&self.tree` 的借用冲突）。
struct PaintContext<'a> {
    geometry: &'a crate::paint::geometry_store::GeometryStore,
    tree: &'a mut NodeTree,
    scene: &'a mut Scene,
    damage: &'a mut DamageAccumulator,
    scale: Scale,
    /// 本帧走全量还是增量路径（共享节点状态逻辑里唯一的分叉点）。
    mode: PaintMode,
    order: &'a mut Vec<LaneId>,
    /// 滚动槽表（全量：本帧重写；增量：按缓存槽号写回，必要时追加）。
    scroll_slots: &'a mut Vec<(Point, PhysicalRect)>,
    /// 槽位 → 容器节点（与 `scroll_slots` 平行，下标 i ⇒ 槽位 i + 1）。
    scroll_slot_nodes: &'a mut Vec<NodeId>,
    focused: Option<NodeId>,
    /// 字形图集（D-5 绘制前硬校验 + E1d 编码期逐 tile 存活校验；`None` = headless/测试路径跳过）。
    atlas: Option<&'a AtlasAllocator>,
    /// 行代际不匹配、本帧未编码的文本节点（D-5）。
    stale_text: SmallVec<[NodeId; 4]>,
    /// Canvas 内含已失效 tile、本帧未编码的字形 run 数（D-5 的 canvas 半边，E1b/E1d）。
    stale_canvas_runs: usize,
    #[cfg(feature = "test-support")]
    log: PaintLog,
    rebuilt: usize,
    /// 本帧做节点状态判定的节点数（`PaintCounters::visited_nodes` 的来源）。
    visited: usize,
    /// 增量路径：本帧是否新建 / 释放过 lane（绘制序成员变化 ⇒ 需要兜底重建）。
    order_changed: bool,
    /// 几何块 lane 重建工作区：旧的烘焙块状态（跨节点/帧复用容量；X5）。
    baked_scratch: SmallVec<[crate::node::tree::BakedGeometryChunk; 2]>,
    /// 几何块 lane 重建工作区：本帧要重建哪些块（与块数等长）。
    rebuild_scratch: SmallVec<[bool; 8]>,
}

/// 屏幕 = 内容 − `total`；图元烘焙 = 内容 − (`total` − `slot_offset`)。
fn offset_between(total: Point, slot_offset: Point) -> Point {
    Point::new(total.x - slot_offset.x, total.y - slot_offset.y)
}

/// 祖先 clip 链（外层在前）+ 物理 covering 交集。
#[derive(Default)]
struct ClipContext {
    rects: SmallVec<[ClipRect; 8]>,
    /// 与 push 次数等长的回溯记录：`Some(prev)` = 这次 push 是折叠（`rects` 未增长，
    /// 需要把栈顶矩形还原成 `prev`）；`None` = 正常压栈，`pop` 时弹掉 `rects` 末尾。
    pushed: SmallVec<[Option<ClipRect>; 8]>,
    covering: Option<PhysicalRect>,
}

impl ClipContext {
    fn key(&self) -> (u8, Option<PhysicalRect>) {
        (self.rects.len() as u8, self.covering)
    }

    /// 进入一个 `Clip::Bounds` 节点：按它的 **padding box** 裁剪子树。
    ///
    /// 深度达到 `MAX_CLIP_DEPTH` 时**折叠**为「与栈顶求交」而不是失败或丢弃（P2-7）：
    /// 轴对齐矩形求交是**无损**的（两个矩形 clip 的复合就是它们的交集），而 `covering`
    /// 本来就是全部矩形的交集。因此 8 层以上嵌套裁剪容器不需要牺牲精度、也不需要让整帧
    /// 失败 —— 旧实现在这里返回 `ClipDepthExceeded`，8 层嵌套的滚动容器会白屏。
    fn push(&mut self, padding_box: Rect, scale: Scale) {
        let clip = ClipRect {
            rect: padding_box,
            corner_radius: [0.0; 4],
        };
        if self.rects.len() >= usize::from(crate::paint::scene::MAX_CLIP_DEPTH) {
            let last = self.rects.last_mut().expect("刚刚判过非空");
            let previous = *last;
            last.rect = last.rect.intersect(clip.rect);
            last.corner_radius = [0.0; 4];
            // 回溯时要把求交前的矩形还原，否则父层的 clip 会被子层的矩形永久收窄
            // （兄弟子树就会被过度裁剪）。
            self.pushed.push(Some(previous));
            self.rebuild_covering(scale);
            return;
        }
        self.rects.push(clip);
        self.pushed.push(None);
        self.rebuild_covering(scale);
    }

    fn pop(&mut self, scale: Scale) {
        match self.pushed.pop() {
            Some(None) => {
                let _ = self.rects.pop();
            }
            Some(Some(previous)) => {
                if let Some(last) = self.rects.last_mut() {
                    *last = previous;
                }
            }
            None => return,
        }
        self.rebuild_covering(scale);
    }

    fn rebuild_covering(&mut self, scale: Scale) {
        let mut covering: Option<PhysicalRect> = None;
        for rect in &self.rects {
            let physical = rect.rect.to_physical_covering(scale);
            covering = Some(match covering {
                Some(parent) => parent.intersect(physical),
                None => physical,
            });
        }
        self.covering = covering;
    }
}

/// 节点要画的内容（读取阶段复制出来，随后释放 `&Node` 借用）。
enum Content {
    Box,
    Text(Option<Rc<ShapedLine>>),
    Canvas(Arc<crate::paint::canvas::Canvas>),
}

/// 该内容是否会产生图元（决定要不要为它压一层内容 clip）。
///
/// `rebuild_lane` 的 clip 深度校验与 `PrimitiveEmitter::emit_content` 必须用**同一个**
/// 判据，否则两处对「这一层占不占深度」的判断不一致。
fn content_needs_clip(content: &Content) -> bool {
    match content {
        Content::Box => false,
        Content::Text(line) => line.is_some(),
        Content::Canvas(_) => true,
    }
}

/// 绘制一棵子树。
///
/// 两个偏移（**滚动合成端偏移的核心约定**，见 `render/delta.rs` 的 [`ScrollTable`]）：
/// - `total`：祖先滚动位移的**完整**累计值（屏幕 = 内容 − total）。damage / clip 上下文 /
///   滚动视口都用它 —— 它们必须留在屏幕坐标里；
/// - `slot_offset`：**最近一个滚动容器**（本子树所属滚动槽）自己的位移。图元烘焙时把它扣掉，
///   于是「滚动这个容器」不改变烘焙字节 ⇒ 不重建 lane、不重编码（`frame.params.yz` 在绘制期
///   把它加回去）。
///
/// `slot` 是本子树所属的滚动槽编号（0 = 不随滚动偏移；`Scene` 的 lane 记录它，编码器据此
/// 生成 `FrameOp::Instances.scroll`）。
///
/// `boundary` 是该槽位的**视口 clip 深度**：`clip.rects[..boundary]` 是「滚动容器之外」的裁剪
/// 链（含容器自己的 padding box）——它们**不**进 lane 的裁剪链（屏幕固定、随滚动在内容坐标里
/// 移动，烘焙进去就等于每滚一像素都要重编码全部 lane），改由槽位的屏幕裁剪区在绘制期做
/// 每 op scissor（`FramePayload::scroll_clip`）。
///
/// [`ScrollTable`]: crate::render::delta::ScrollTable
///
/// # 增量剪枝（A1）
///
/// `force` = 本节点所在的子树被「必须完整访问」的口径覆盖：全量路径恒真；增量路径的根为假，
/// 但从**非 reusable** 节点向下递归时置真（该节点的 clip 上下文 / 隐藏状态可能变化，整棵子树
/// 都可能需要重建 —— 这是唯一需要保守展开的地方）。未被覆盖且不在脏闭包里的节点整棵跳过，
/// 它上一帧的 lane / 缓存保持不变（它们本来就是 reusable 的）。
fn paint_node(
    context: &mut PaintContext<'_>,
    node: NodeId,
    parent_hidden: bool,
    clip: &mut ClipContext,
    total: Point,
    slot_offset: Point,
    slot: u32,
    boundary: usize,
    force: bool,
) -> Result<(), EngineError> {
    if !force && !context.tree.paint_marked(node) {
        return Ok(());
    }
    context.visited += 1;
    // 一次取全本节点的只读状态（不可见 / 复用判据 / lane / 子节点数 / clip 盒 / 滚动偏移）。
    //
    // 不可见的**定义只有一处**：`display: none` 或 `visible: false`。命中测试、焦点遍历、
    // a11y 都用同一规则，并且都按「隐藏整棵子树」处理；旧实现的 paint 只对 `display: none`
    // 隐藏子树，`visible: false` 仅跳过本节点自身的图元 —— 于是容器的背景消失、子控件仍然
    // 画出来，而那些子控件在命中测试里是不可见的（「看得见但点不动」的死像素，P2-3.2）。
    //
    // 复用判据（§5.6 + P10 §7.6）：`REUSABLE` + clip 上下文一致 + **烘焙偏移戳一致**（祖先滚动会
    // 移动全部后代图元，因此偏移变化必须重建；滚动**本槽**不改变烘焙偏移 ⇒ 不重建）。
    // **不要求** lane 存在：从未产生图元的节点没有 lane，但它的 `clip_key` 已写回，仍可整帧跳过。
    let current_key = clip.key();
    let Some(state) =
        context
            .tree
            .paint_state(node, total, offset_between(total, slot_offset), current_key)
    else {
        return Ok(());
    };
    if parent_hidden || state.hidden {
        hide_subtree(context, node, clip, total);
        return Ok(());
    }
    // 自绘节点（几何 / Canvas）必须留在屏幕坐标：`FrameOp::Geometry` 没有滚动槽、Canvas 图元
    // 也不参与合成端平移（应用给的是绝对坐标）。它们照旧每次滚动都整条重建 —— 正确性优先。
    let (node_slot, node_slot_offset) = if state.screen_baked {
        (0, Point::ZERO)
    } else {
        (slot, slot_offset)
    };
    // 复用判据里的槽位一致：树结构变化会重新编号槽位（段的归属变了 ⇒ 必须重编码）。
    let reusable = state.reusable
        && state
            .lane
            .is_none_or(|lane| context.scene.lane_scroll(lane) == node_slot);
    if let Some(geometry) = state.geometry {
        // 几何节点：按块 lane 重建（X5）。数据 range 更新只把受影响块的版本推进，因此这里
        // 只重建版本变化的块 lane —— 追加 K 线只重建尾块，热力图改色只重建受影响块。
        paint_geometry_node(context, node, &state, geometry, clip, total)?;
    } else if reusable {
        // 增量路径不 push 绘制序：顺序没变（结构未变、lane 成员未变），`Scene::order` 整体保留。
        if context.mode.is_full()
            && let Some(lane) = state.lane
        {
            context.order.push(lane);
        }
        #[cfg(feature = "test-support")]
        if let Some(lane) = state.lane {
            let primitives = context
                .scene
                .lane_primitives(lane)
                .map_or(0, <[Primitive]>::len);
            context.log.lanes.push(LanePaintEntry {
                lane,
                node,
                outcome: LaneOutcome::Reused,
                primitives,
            });
        }
    } else {
        rebuild_lane(
            context,
            node,
            clip,
            total,
            node_slot,
            node_slot_offset,
            boundary,
        )?;
    }

    // B1：0×0 的 padding box **也要入栈**（depth+1、covering 交空）——子 lane 由此带空前缀、
    // damage 全部被裁空，与 `hit_test` 的 clip 链语义一致；跳过它会让子内容"越界可见"。
    let child_count = state.children;
    let clip_bounds = state.clip_bounds;
    if let Some(padding_box) = clip_bounds {
        // 深度上限由 `ClipContext::push` 自己处理：达到上限时与栈顶矩形求交（无损），
        // 既不失败也不丢弃（P2-7）。
        clip.push(padding_box, context.scale);
    }

    // 本节点是滚动容器 ⇒ 给它的子树分配一个槽位，并把「本槽位移」从现在起从烘焙里扣掉。
    // 全量路径按绘制序**稠密编号**并写进节点缓存（树不变 ⇒ 逐帧稳定）；增量路径读缓存编号，
    // 不重排已有槽位（已烘焙进 lane 的槽号不能动），缓存缺失时追加新槽。
    // 槽位的**屏幕裁剪区**是「容器 padding box 与全部祖先 clip 的交集」（= 刚 push 完之后的
    // covering），它同时是滚动 damage 与 GPU 侧该槽全部 op 的 scissor —— 因此滚动容器的视口
    // clip 不再进 lane 的裁剪链。
    let mut child_total = total;
    let mut child_slot = node_slot;
    let mut child_slot_offset = node_slot_offset;
    let mut child_boundary = boundary;
    if let Some(own) = state.scroll {
        let viewport = clip.covering.unwrap_or_else(|| {
            state.clip_bounds.map_or_else(
                || PhysicalRect::new(0, 0, 0, 0),
                |rect| rect.to_physical_covering(context.scale),
            )
        });
        let index = scroll_slot_for(context, node, own, viewport);
        child_total = Point::new(total.x + own.x, total.y + own.y);
        child_slot = index;
        child_slot_offset = own;
        // 视口 clip 已经「花掉」：子树看到的 `clip.rects[..boundary]` 全部由屏幕裁剪区承担。
        child_boundary = clip.rects.len();
    }
    // 非 reusable 的节点会让整棵子树的 clip 上下文 / 隐藏状态可能与上一帧不同 ⇒ 增量路径
    // 保守展开（全量路径本来就访问全部子节点）。
    let expand = context.mode.is_full() || !reusable;
    for index in 0..child_count {
        let child = match context.tree.get(node) {
            Ok(target) => target.children()[index],
            Err(_) => break,
        };
        paint_node(
            context,
            child,
            false,
            clip,
            child_total,
            child_slot_offset,
            child_slot,
            child_boundary,
            expand,
        )?;
    }
    if clip_bounds.is_some() {
        clip.pop(context.scale);
    }
    Ok(())
}

/// 本节点的滚动槽：全量路径稠密编号并缓存；增量路径复用缓存（缺失时追加新槽，不重排）。
///
/// 增量路径的「追加」是槽位表**只增不乱**的唯一语义：已烘焙进 lane 的槽号必须保持不变，
/// 隐藏容器留下的旧槽（没有 lane 引用）留在表里无害；下一次全量路径会重新稠密编号。
fn scroll_slot_for(
    context: &mut PaintContext<'_>,
    node: NodeId,
    offset: Point,
    viewport: PhysicalRect,
) -> u32 {
    let cached = context.tree.scroll_slot_cached(node);
    if context.mode.is_full() || cached == 0 {
        let index = u32::try_from(context.scroll_slots.len()).unwrap_or(u32::MAX);
        context.scroll_slots.push((offset, viewport));
        context.scroll_slot_nodes.push(node);
        context.tree.set_scroll_slot(node, index);
        return index;
    }
    let slot = cached as usize;
    match slot.checked_sub(1) {
        Some(parallel) if parallel < context.scroll_slot_nodes.len() => {
            context.scroll_slot_nodes[parallel] = node;
        }
        _ => {
            // 表比缓存短（只可能来自「恢复的缓存 + 被截短的表」，防御性追加）：
            // 记一个新槽，保持 `scroll_slots.len() == scroll_slot_nodes.len() + 1` 的不变量。
            let index = u32::try_from(context.scroll_slots.len()).unwrap_or(u32::MAX);
            context.scroll_slots.push((offset, viewport));
            context.scroll_slot_nodes.push(node);
            context.tree.set_scroll_slot(node, index);
            return index;
        }
    }
    if let Some(slot) = context.scroll_slots.get_mut(slot) {
        *slot = (offset, viewport);
    }
    cached
}

/// 增量路径的绘制序兜底：只读 `children` + `lane_of` 重建 DFS 先序（不碰 clip / damage / 图元）。
///
/// 触发条件：增量帧里新建或释放了 lane（绘制序成员变化）——这是唯一会让 `Scene::order` 变化
/// 的增量情形（结构变化走全量路径）。隐藏节点不产生绘制输出，它们的空 lane 不进序。
fn rebuild_order_only(tree: &NodeTree, order: &mut Vec<LaneId>) {
    order.clear();
    let mut stack: SmallVec<[(NodeId, bool); 32]> = SmallVec::new();
    stack.push((tree.root(), false));
    while let Some((node, parent_hidden)) = stack.pop() {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        let own_hidden = matches!(target.style().inner.display, taffy::style::Display::None)
            || target.style_patch().visible == Some(false);
        if parent_hidden || own_hidden {
            // 隐藏子树：`hide_subtree` 已把它的 lane 清空，全量路径同样不会把它计入绘制序。
            continue;
        }
        // 几何节点按块序 push 全部块 lane（X5：块间序 = 块序 = 索引顺序）；普通节点 0/1 条。
        for lane in target.lanes() {
            order.push(*lane);
        }
        for child in target.children().iter().rev() {
            stack.push((*child, false));
        }
    }
}

/// 隐藏子树：清空每条 lane（保留分配），并把旧像素并入 damage；`clip_key` 置空
/// （再次显示时必然失配 ⇒ 重建，避免"复用空 lane"）。
///
/// `total` 是完整的祖先滚动位移（damage 是屏幕坐标）；隐藏子树不产生 lane，因此不需要烘焙偏移。
fn hide_subtree(context: &mut PaintContext<'_>, node: NodeId, clip: &ClipContext, total: Point) {
    // AR5 P2-1：隐藏同样是"内容消失"——旧 lane 里的文本/Canvas 可见范围可能溢出节点盒，
    // 必须先记下再清 lane，否则 `Clip::None` 的长文本隐藏后旧字形留在屏幕上。
    let previous_content = painted_content(context.tree, context.scene, node);
    let (children, damage_rect, child_offset) = {
        let Ok(target) = context.tree.get(node) else {
            return;
        };
        let children: SmallVec<[NodeId; 4]> = target.children().iter().copied().collect();
        // damage 与 bounds 都在**内容坐标**：先并集，再减祖先滚动偏移得到屏幕坐标（P10 §7.2）。
        let mut damage_rect = target
            .damage()
            .union(target.layout().bounds)
            .union(previous_content.content);
        // 几何的可见范围可能超出节点布局盒：隐藏时也必须把 `desc.bounds` 并入
        // damage，否则溢出部分留下旧像素（「damage 帧不清屏」会长期残留；复审 A3）。
        if let NodeKind::Geometry(id) = *target.kind()
            && let Some(snapshot) = context.geometry.snapshot_arc(id)
        {
            damage_rect = damage_rect.union(snapshot.bounds());
        }
        // damage 与 bounds 都在**内容坐标**：先并集，再减祖先滚动偏移得到屏幕坐标（P10 §7.2）。
        let damage_rect = damage_rect.translate(negate(total));
        let child_offset = total
            + context
                .tree
                .scroll_slot(node)
                .map_or(Point::ZERO, |slot| slot.offset);
        (children, damage_rect, child_offset)
    };
    // 几何节点可能有多条块 lane（X5）：隐藏时全部清空（保留分配，再次显示时重建）。
    for index in 0..context.tree.lane_count(node) {
        let Some(lane) = context.tree.lane_at(node, index) else {
            continue;
        };
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        if had_primitives {
            let _ = context.scene.clear_lane(lane);
            context.rebuilt += 1;
            // 全量路径重建绘制序：清空内容但**保留 lane 分配**的节点仍占一个绘制序位置
            // （GPU 侧据此把该段实例清掉）。增量路径的顺序沿用上一帧，该 lane 已经在序里
            // （「有图元的 lane 必在绘制序里」是不变式）⇒ 不 push、也不算顺序变化。
            if context.mode.is_full() {
                context.order.push(lane);
            }
            #[cfg(feature = "test-support")]
            context.log.lanes.push(LanePaintEntry {
                lane,
                node,
                outcome: LaneOutcome::Cleared,
                primitives: 0,
            });
        }
    }
    context.tree.clear_clip_key(node);
    context.tree.set_reusable(node);
    context.tree.clear_paint(node);
    context.damage.push(damage_rect, 1.0, clip.covering);
    if !previous_content.screen.is_empty() {
        context
            .damage
            .push(previous_content.screen, 1.0, clip.covering);
    }
    for child in children {
        hide_subtree(context, child, clip, child_offset);
    }
}

/// 重建一条 lane（该节点本帧有变化 / 首次出现 / clip 或烘焙偏移失配）。
///
/// `total` = 完整祖先滚动位移（damage 用，屏幕坐标）；
/// `slot` / `slot_offset` = 该 lane 的滚动槽位与**本槽自己的位移**（图元烘焙时扣掉它，
/// 绘制期由 GPU 加回来；见 `paint_node` 的文档）。
fn rebuild_lane(
    context: &mut PaintContext<'_>,
    node: NodeId,
    clip: &ClipContext,
    total: Point,
    slot: u32,
    slot_offset: Point,
    boundary: usize,
) -> Result<(), EngineError> {
    let created = context.tree.lane_of(node).is_none();
    let lane = match context.tree.lane_of(node) {
        Some(lane) => lane,
        None => {
            let lane = context.scene.new_lane();
            context.tree.set_lane(node, lane);
            lane
        }
    };
    // AR5 P2-1：旧可见内容范围必须在清空 lane **之前**记下 —— 内容收缩（长文本变短 /
    // Canvas 变小）后新图元只覆盖收缩后的区域，旧像素需要这份旧 AABB 才会被重画。
    let previous_content = painted_content(context.tree, context.scene, node);
    // 增量路径的绘制序成员判据：清空前的「这条 lane 里有没有图元」。
    let had_primitives = context
        .scene
        .lane_primitives(lane)
        .is_some_and(|primitives| !primitives.is_empty());
    let _ = context.scene.clear_lane(lane);
    // 槽位必须写进 lane：编码器据此生成 `FrameOp::Instances.scroll`，也是段复用的判据之一。
    // `set_lane_scroll` 在槽位变化时让内容版本 +1，因此总线上不会漏掉这条 lane。
    let _ = context.scene.set_lane_scroll(lane, slot);
    // 烘焙偏移 = total − slot_offset（不含本槽自己的位移）。
    let bake = offset_between(total, slot_offset);

    let (
        bounds,
        content_offset,
        patch,
        spec,
        content,
        node_damage,
        ring,
        content_clip,
        geometry_transform,
    ) = {
        let Ok(target) = context.tree.get(node) else {
            return Ok(());
        };
        let content = match target.kind() {
            NodeKind::Box => Content::Box,
            NodeKind::Text(_) => Content::Text(context.tree.text_line(node).cloned()),
            NodeKind::Canvas(canvas) => Content::Canvas(Arc::clone(canvas)),
            // 几何节点在 paint_node 就分流到 paint_geometry_node（块 lane 路径），
            // 不会走到单 lane 的重建里；这里只是满足 match 的穷尽性。
            NodeKind::Geometry(_) => Content::Box,
        };
        // 烘焙坐标 = 布局坐标 − 祖先滚动偏移（P10 §7.2；**不含本槽自己的位移**）。
        let bounds = target.layout().bounds.translate(negate(bake));
        let patch = target.style_patch().clone();
        let spec = target.paint_spec().cloned();
        let node_damage = target.damage().union(previous_content.content);
        let content_offset = context.tree.content_offset(node);
        let content_clip =
            (target.clip() == Clip::Bounds).then(|| bounds.inset(target.layout().border));
        let ring = (context.focused == Some(node))
            .then(|| context.tree.focus_ring(node))
            .flatten();
        // 普通内容（Box/Text/Canvas）没有 per-draw 变换；几何节点的变换由
        // paint_geometry_node 单独读取（X5）。
        let geometry_transform = crate::math::Transform::IDENTITY;
        (
            bounds,
            content_offset,
            patch,
            spec,
            content,
            node_damage,
            ring,
            content_clip,
            geometry_transform,
        )
    };

    // 内容 clip 的层数由 `ClipContext::push` 折叠处理（达到上限时求交而不是失败），
    // 这里不需要再校验深度：8 层以上的嵌套裁剪容器会退化成一条求交后的矩形。
    //
    // **坐标空间**：`ClipContext` 里的矩形是屏幕坐标（与 damage 同源），而 lane 的图元是烘焙
    // 坐标（不含本槽位移）⇒ 压进 lane 的 clip 必须同样加上本槽位移（烘焙 = 屏幕 + slot_offset）。
    // 两者同处烘焙空间，片元侧的 `clip_mask`（在烘焙坐标上判定）因此与图元天然一致。
    //
    // **槽位边界**：`clip.rects[..boundary]` 是滚动容器之外的裁剪链（含容器自己的视口）——
    // 它在内容坐标里随滚动移动，烘焙进去就要求每滚一像素重编码全部 lane。它由槽位的屏幕
    // 裁剪区在绘制期做每 op scissor（`FramePayload::scroll_clip`），这里必须跳过。
    let inner_clips = if slot == 0 {
        &clip.rects[..]
    } else {
        &clip.rects[boundary.min(clip.rects.len())..]
    };
    for clip_rect in inner_clips {
        let mut baked = *clip_rect;
        baked.rect = baked.rect.translate(slot_offset);
        let _ = context
            .scene
            .push(lane, Primitive::Clip(ClipOp::Push(baked)));
    }

    let mut emitter = PrimitiveEmitter {
        node,
        lane,
        bounds,
        content_offset,
        patch,
        spec,
        scale: context.scale,
        clip: clip.covering,
        content,
        emitted: 0,
        node_damage,
        offset: total,
        damage_shift: negate(slot_offset),
        ring,
        content_clip,
        geometry_transform,
    };
    emitter.emit(context);

    // Canvas 的旧可见范围是屏幕坐标（`screen_baked`，不参与合成端平移），直接进 damage。
    if !previous_content.screen.is_empty() {
        context
            .damage
            .push(previous_content.screen, 1.0, clip.covering);
    }

    for _ in inner_clips.iter().rev() {
        let _ = context.scene.push(lane, Primitive::Clip(ClipOp::Pop));
    }

    let primitives = context
        .scene
        .lane_primitives(lane)
        .map_or(0, <[Primitive]>::len);
    context.tree.set_reusable(node);
    context.tree.set_clip_key(node, clip.key());
    // 烘焙偏移戳（**不含本槽位移**）：滚动本容器不改变它 ⇒ 下一帧仍可整条复用。
    context.tree.set_lane_offset(node, bake);
    context.tree.clear_paint(node);

    // 「没有东西要画」的节点不留 lane（空 lane 只占绘制序/内存）：丢弃句柄并释放分配。
    // 它的 `clip_key` 已写回，下一帧仍可命中复用判据（无需每帧重建）。
    if primitives == 0 {
        context.scene.release_lane(lane);
        context.tree.clear_lane(node);
        if !emitter.node_damage.is_empty() {
            context.rebuilt += 1;
        }
        // 增量路径：释放了一条**本来在绘制序里**的 lane ⇒ 顺序成员变化，遍历结束后兜底重建。
        if !context.mode.is_full() && (created || had_primitives) {
            context.order_changed = true;
        }
    } else {
        context.rebuilt += 1;
        if context.mode.is_full() {
            context.order.push(lane);
        } else if created || !had_primitives {
            // 增量路径：新建 lane，或复活了一条没有图元（不在序里）的旧 lane ⇒ 顺序成员变化。
            context.order_changed = true;
        }
    }

    #[cfg(feature = "test-support")]
    context.log.lanes.push(LanePaintEntry {
        lane,
        node,
        outcome: if primitives == 0 {
            LaneOutcome::Cleared
        } else if created {
            LaneOutcome::Allocated
        } else {
            LaneOutcome::Rebuilt
        },
        primitives,
    });
    Ok(())
}

/// 几何节点一次 paint 需要的绘制状态（与 `rebuild_lane` 的读取块同源；X5）。
struct GeometryNodeState {
    /// 节点盒的**烘焙坐标**（几何节点 = 屏幕坐标）。
    bounds: Rect,
    content_offset: Point,
    patch: StylePatch,
    spec: Option<PaintSpec>,
    node_damage: Rect,
    ring: Option<crate::paint::style::FocusRing>,
    content_clip: Option<Rect>,
    geometry_transform: crate::math::Transform,
    /// 图元烘焙偏移（几何节点 = 祖先滚动累计值，因为它是 screen-baked 节点）。
    bake: Point,
}

/// 读一个几何节点的全部绘制状态（一次 arena 查找 + 少量旁表查；消费者：
/// [`paint_geometry_node`]）。
fn geometry_node_state(
    context: &PaintContext<'_>,
    node: NodeId,
    total: Point,
    node_slot_offset: Point,
) -> Option<GeometryNodeState> {
    let Ok(target) = context.tree.get(node) else {
        return None;
    };
    let bake = offset_between(total, node_slot_offset);
    let bounds = target.layout().bounds.translate(negate(bake));
    let patch = target.style_patch().clone();
    let spec = target.paint_spec().cloned();
    let node_damage = target.damage();
    let content_offset = context.tree.content_offset(node);
    let content_clip =
        (target.clip() == Clip::Bounds).then(|| bounds.inset(target.layout().border));
    let ring = (context.focused == Some(node))
        .then(|| context.tree.focus_ring(node))
        .flatten();
    let geometry_transform = context.tree.geometry_transform(node);
    Some(GeometryNodeState {
        bounds,
        content_offset,
        patch,
        spec,
        node_damage,
        ring,
        content_clip,
        geometry_transform,
        bake,
    })
}

/// 某块在**逻辑空间**的 AABB（快照块表；空块退回整块几何 `bounds`）。`None` = 未注册句柄。
///
/// **缺失块**（`truncate` 到 0 块时 paint 仍保留一条装饰 lane）不回退整块几何 `bounds`：
/// 这条 lane 里没有几何内容，退回注册范围会把整个几何 bounds 并入 damage（清空图表时闪一次
/// 过大 damage）。未注册句柄的退化（`None`）仍由调用方用节点盒 / clip 处理。
fn geometry_chunk_local_bounds(
    snapshot: Option<&std::sync::Arc<crate::paint::geometry::GeometrySnapshot>>,
    chunk: usize,
) -> Option<Rect> {
    let snapshot = snapshot?;
    Some(match snapshot.chunk(chunk as u32) {
        Some(meta) if !meta.bounds.is_empty() => meta.bounds,
        Some(_) => snapshot.bounds(),
        None => Rect::default(),
    })
}

/// 几何节点不可见 / 零尺寸：释放全部块 lane、清空块状态，并把旧块范围并入 damage。
fn clear_geometry_lanes(
    context: &mut PaintContext<'_>,
    node: NodeId,
    old_lanes: usize,
    total: Point,
    clip: &ClipContext,
    node_damage: &Rect,
    geometry_transform: crate::math::Transform,
    bake: Point,
) {
    for chunk in 0..old_lanes {
        let Some(previous) = context.baked_scratch.get(chunk).copied() else {
            continue;
        };
        let damage = geometry_chunk_damage(geometry_transform, previous.bounds, total);
        if !damage.is_empty() {
            context.damage.push(damage, 1.0, clip.covering);
        }
    }
    let mut order_changed = false;
    for chunk in 0..old_lanes {
        let Some(lane) = context.tree.lane_at(node, chunk) else {
            continue;
        };
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        if !context.mode.is_full() && had_primitives {
            order_changed = true;
        }
        context.scene.release_lane(lane);
        #[cfg(feature = "test-support")]
        context.log.lanes.push(LanePaintEntry {
            lane,
            node,
            outcome: LaneOutcome::Cleared,
            primitives: 0,
        });
    }
    context.tree.clear_lane(node);
    context.tree.resize_baked_geometry_chunks(node, 0);
    if !node_damage.is_empty() {
        context
            .damage
            .push(node_damage.translate(negate(total)), 1.0, clip.covering);
        context.rebuilt += 1;
    }
    context.tree.set_reusable(node);
    context.tree.set_clip_key(node, clip.key());
    context.tree.set_lane_offset(node, bake);
    context.tree.clear_paint(node);
    if order_changed {
        context.order_changed = true;
    }
}

/// 几何节点的 paint（X5）：按**块 lane** 规划绘制输出，只重建数据版本变化的块。
///
/// # 契约（与单 lane 完全一致）
///
/// - **块内/块间绘制序**：块 lane 在绘制序里连续（`lanes[0..n]`，按块序），块内图元序 =
///   祖先 clip 前缀 → 块 0 lane 的装饰（Shadow/背景/边框/焦点环）→ 内容 clip → 几何 →
///   content clip 后缀 → 祖先 clip 后缀；块 `i>0` 的 lane 只有「祖先 clip 前缀 + 内容 clip +
///   第 i 块几何 + 后缀」。与旧单 lane 的差异只是「内容」被切到多条 lane，绘制顺序不变。
/// - **clip 语义**：每条块 lane 都带完整的祖先 clip 前缀/后缀 + 内容 clip 括号（`emit_content`
///   的同一实现），因此裁剪结果与单 lane 逐像素一致。
/// - **复用**：`REUSABLE` + clip 上下文 + 烘焙偏移一致的前提下，只有**块版本**（几何数据里
///   该块的 revision）变化的块 lane 被重建。data-only 更新（`mark_geometry_updated`）不清
///   `REUSABLE`，于是追加 K 线只重建尾块、热力图改色只重建受影响块；纯滚动帧仍然
///   `encoded_lanes == 0`、`delta.order == None`。
/// - **damage**：每条被重建的块 lane 把「旧块 bounds ∪ 新块 bounds」（经 per-draw 变换）
///   并入 damage；块数增加/减少同样按旧 bounds 记 damage。
///
/// # Panics
/// debug 构建断言几何节点是 screen-baked（`paint_node` 恒传 slot 0 / offset 0）。
fn paint_geometry_node(
    context: &mut PaintContext<'_>,
    node: NodeId,
    state: &PaintNodeState,
    geometry_id: crate::paint::geometry::GeometryId,
    clip: &ClipContext,
    total: Point,
) -> Result<(), EngineError> {
    let snapshot = context.geometry.snapshot_arc(geometry_id);
    // 块数：未注册句柄保守按 1 块；已注册但 **0 块**（`truncate` 到 0 / 空几何）也保留 1 条
    // 装饰 lane —— 几何 op 因 `snapshot.chunk(0)` 缺失而不发射，但节点自己的背景 / 边框 /
    // 焦点环 / 节点盒 damage 仍需要一条 lane 承载（空数据不应让节点样式消失）。
    let desired = snapshot
        .as_ref()
        .map_or(1, |snapshot| snapshot.chunks().len().max(1));
    // 一次 arena 查找取全本节点的绘制状态（与 `rebuild_lane` 同一份读取块）。
    let Some(node_state) = geometry_node_state(context, node, total, Point::ZERO) else {
        return Ok(());
    };
    let GeometryNodeState {
        bounds,
        content_offset,
        patch,
        spec,
        node_damage,
        ring,
        content_clip,
        geometry_transform,
        bake,
    } = node_state;
    let opacity = match patch.opacity {
        Some(value) if value.is_nan() => 0.0,
        Some(value) => value.clamp(0.0, 1.0),
        None => 1.0,
    };
    let visible = patch.visible.unwrap_or(true);
    let old_lanes = context.tree.lane_count(node);
    // 旧块烘焙状态（决定「哪些块需要重建」与旧 bounds damage），拷进复用工作区。
    context.baked_scratch.clear();
    if let Some(baked) = context.tree.baked_geometry_chunks(node) {
        context.baked_scratch.extend(baked.iter().copied());
    }
    // 节点级 gate：不可见 / 零尺寸布局盒 ⇒ 不画任何块（旧实现 `PrimitiveEmitter::emit` 的
    // 早退语义），清空全部块 lane 并把旧块范围并入 damage。
    if !visible || bounds.is_empty() {
        clear_geometry_lanes(
            context,
            node,
            old_lanes,
            total,
            clip,
            &node_damage,
            geometry_transform,
            bake,
        );
        return Ok(());
    }
    // 规划：块数变化 / 节点级重建（`REUSABLE` 失配）⇒ 全部块重建；否则只重建版本变化的块。
    context.rebuild_scratch.clear();
    for chunk in 0..desired {
        let lane_missing = context.tree.lane_at(node, chunk).is_none();
        let revision = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.chunk(chunk as u32))
            .map_or(0, |meta| meta.revision);
        let baked_matches = context
            .baked_scratch
            .get(chunk)
            .is_some_and(|baked| baked.revision == revision);
        context
            .rebuild_scratch
            .push(!state.reusable || lane_missing || !baked_matches);
    }
    // 先对齐块状态的条目数（复用块保留原值；新块由上面的规划判为重建）。
    context.tree.resize_baked_geometry_chunks(node, desired);
    let mut created_any = false;
    // 内层 clip（屏幕 → 烘焙坐标；几何节点恒 slot 0：见 `paint_node` 的 screen_baked 分支）。
    let inner_clips = &clip.rects[..];
    for chunk in 0..desired {
        let lane = match context.tree.lane_at(node, chunk) {
            Some(lane) => lane,
            None => {
                let lane = context.scene.new_lane();
                context.tree.push_lane(node, lane);
                created_any = true;
                lane
            }
        };
        if !context.rebuild_scratch[chunk] {
            if context.mode.is_full() {
                context.order.push(lane);
            }
            #[cfg(feature = "test-support")]
            {
                let primitives = context
                    .scene
                    .lane_primitives(lane)
                    .map_or(0, <[Primitive]>::len);
                context.log.lanes.push(LanePaintEntry {
                    lane,
                    node,
                    outcome: LaneOutcome::Reused,
                    primitives,
                });
            }
            continue;
        }
        // 旧块 bounds 必须在清空 lane 之前记下（内容收缩 / 块删除时旧像素要重画）。
        let previous = context.baked_scratch.get(chunk).copied();
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        let _ = context.scene.clear_lane(lane);
        let _ = context.scene.set_lane_scroll(lane, 0);
        context
            .scene
            .set_lane_geometry_chunk(lane, Some((geometry_id, chunk as u32)));
        context.tree.set_lane_offset(node, bake);
        for clip_rect in inner_clips {
            let _ = context
                .scene
                .push(lane, Primitive::Clip(ClipOp::Push(*clip_rect)));
        }
        let mut emitter = PrimitiveEmitter {
            lane,
            node,
            bounds,
            content_offset,
            patch: patch.clone(),
            spec: spec.clone(),
            scale: context.scale,
            clip: clip.covering,
            content: Content::Box,
            emitted: 0,
            node_damage: if chunk == 0 {
                node_damage
            } else {
                Rect::default()
            },
            offset: total,
            damage_shift: negate(Point::ZERO),
            ring: if chunk == 0 { ring } else { None },
            geometry_transform,
            content_clip,
        };
        if chunk == 0 {
            emitter.emit_decorations(context, opacity);
        }
        emitter.emit_geometry_chunk(context, geometry_id, chunk as u32, opacity);
        if chunk == 0 {
            emitter.emit_focus_ring(context, opacity);
            emitter.emit_node_damage(context);
        }
        for _ in inner_clips.iter().rev() {
            let _ = context.scene.push(lane, Primitive::Clip(ClipOp::Pop));
        }
        // 全量路径的绘制序在同一次遍历里生成（增量路径不 push：顺序不变；新建 lane 由
        // 遍历结束后的轻量 DFS 兜底重建）。
        if context.mode.is_full() {
            context.order.push(lane);
        }
        // damage = 旧块 bounds ∪ 新块 bounds（都经 per-draw 变换；旧一侧来自烘焙状态）。
        let mut damage = match (
            &snapshot,
            geometry_chunk_local_bounds(snapshot.as_ref(), chunk),
        ) {
            // 未注册句柄：旧实现退回节点布局盒（拿不到真实范围）。
            (None, _) => bounds,
            (Some(_), Some(new_bounds)) => {
                geometry_chunk_damage(geometry_transform, new_bounds, total)
            }
            (Some(_), None) => Rect::default(),
        };
        if let Some(previous) = previous {
            damage = damage.union(geometry_chunk_damage(
                geometry_transform,
                previous.bounds,
                total,
            ));
        }
        if !damage.is_empty() {
            context.damage.push(damage, 1.0, clip.covering);
        }
        let revision = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.chunk(chunk as u32))
            .map_or(0, |meta| meta.revision);
        let baked_bounds =
            geometry_chunk_local_bounds(snapshot.as_ref(), chunk).unwrap_or_default();
        context.tree.set_baked_geometry_chunk(
            node,
            chunk,
            BakedGeometryChunk {
                revision,
                bounds: baked_bounds,
            },
        );
        context.rebuilt += 1;
        #[cfg(feature = "test-support")]
        {
            let primitives = context
                .scene
                .lane_primitives(lane)
                .map_or(0, <[Primitive]>::len);
            context.log.lanes.push(LanePaintEntry {
                lane,
                node,
                outcome: if had_primitives {
                    LaneOutcome::Rebuilt
                } else {
                    LaneOutcome::Allocated
                },
                primitives,
            });
        }
    }
    // 块数减少（当前 API 只能增长，保留这条路径给「重置/截断」的后续语义）：释放多余 lane。
    for chunk in desired..old_lanes {
        let Some(lane) = context.tree.lane_at(node, chunk) else {
            continue;
        };
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        if let Some(previous) = context.baked_scratch.get(chunk) {
            let damage = geometry_chunk_damage(geometry_transform, previous.bounds, total);
            if !damage.is_empty() {
                context.damage.push(damage, 1.0, clip.covering);
            }
        }
        context.scene.release_lane(lane);
        context.rebuilt += 1;
        if !context.mode.is_full() && had_primitives {
            context.order_changed = true;
        }
    }
    // 截断 `Node.lanes` / 块状态到 `desired`。
    context.tree.truncate_lanes(node, desired);
    context.tree.resize_baked_geometry_chunks(node, desired);
    context.tree.set_reusable(node);
    context.tree.set_clip_key(node, clip.key());
    context.tree.set_lane_offset(node, bake);
    context.tree.clear_paint(node);
    if !context.mode.is_full() && created_any {
        context.order_changed = true;
    }
    Ok(())
}

/// 几何块 lane 的 damage 范围：`transform(块 AABB)` 换算到屏幕坐标（减祖先滚动累计值）。
///
/// 块 bounds 是**逻辑像素**（几何局部空间，与 `GeometryDesc.bounds` 同空间），因此要先过
/// per-draw 变换再减滚动累计值 —— 与旧单 lane 的 `geometry_damage` 同一算法；顶点位移时
/// 调用方把「旧块 bounds」也过一次同样的变换，两者并集就是「旧位置 ∪ 新位置」。
fn geometry_chunk_damage(
    transform: crate::math::Transform,
    chunk_bounds: Rect,
    total: Point,
) -> Rect {
    if chunk_bounds.is_empty() {
        return Rect::default();
    }
    let aabb = transformed_chunk_aabb(transform, chunk_bounds);
    Rect::new(
        aabb[0] - total.x,
        aabb[1] - total.y,
        (aabb[2] - aabb[0]).max(0.0),
        (aabb[3] - aabb[1]).max(0.0),
    )
}

/// 逻辑矩形经仿射变换后的 AABB（`[x0, y0, x1, y1]`；非有限输入 = 空区间）。
fn transformed_chunk_aabb(transform: crate::math::Transform, bounds: Rect) -> [f32; 4] {
    let far = Point::new(
        bounds.origin.x + bounds.size.width,
        bounds.origin.y + bounds.size.height,
    );
    let corners = [
        transform.apply(bounds.origin),
        transform.apply(Point::new(far.x, bounds.origin.y)),
        transform.apply(Point::new(bounds.origin.x, far.y)),
        transform.apply(far),
    ];
    if !corners
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
    {
        return [1.0, 1.0, -1.0, -1.0];
    }
    let mut aabb = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for point in corners {
        aabb = [
            aabb[0].min(point.x),
            aabb[1].min(point.y),
            aabb[2].max(point.x),
            aabb[3].max(point.y),
        ];
    }
    aabb
}

struct PrimitiveEmitter {
    /// 发出本节点图元的节点 id（D-5：图集代际不匹配时记入 `stale_text` 重新入队）。
    node: NodeId,
    lane: LaneId,
    bounds: Rect,
    content_offset: Point,
    patch: StylePatch,
    spec: Option<PaintSpec>,
    scale: Scale,
    clip: Option<PhysicalRect>,
    content: Content,
    /// 已 push 的图元数（决定本节点是否计入 `rebuilt`/damage）。
    emitted: usize,
    /// 本帧累积的节点级 damage（**内容坐标**；`touch`/`absolutize` 写入）。
    node_damage: Rect,
    /// 该节点的**完整**祖先滚动位移（把 `node_damage` 换算到屏幕坐标）。
    offset: Point,
    /// 烘焙坐标 → 屏幕坐标的平移（= −本槽位移；`0` = 不在滚动容器里）。
    ///
    /// 图元的 `rect` 是烘焙坐标，而 damage 必须是屏幕坐标（scissor / 平台 present 都用屏幕），
    /// 因此由 [`PrimitiveEmitter::push_damage`] 统一加上这个平移。
    damage_shift: Point,
    /// 焦点环（仅焦点节点 + `Style.focus_ring` 时存在）。
    ring: Option<crate::paint::style::FocusRing>,
    /// 本节点几何内容的 per-draw 变换（R05-12；`Content::Geometry` 才有意义）。
    geometry_transform: crate::math::Transform,
    /// 本节点**自身**的 `Clip::Bounds` padding box（屏幕坐标）。
    ///
    /// 只作用于**内容**（文本 / Canvas / 几何），不作用于背景/边框/阴影 —— 与 CSS 一致：
    /// `overflow` 裁剪的是元素内容，不是元素自己的装饰。旧实现只把 clip 压给子树，
    /// 于是「容器剪不住自己的 Canvas」：图表的柱子会溢出到容器外（P2-3.2）。
    content_clip: Option<Rect>,
}

/// 取负（`Point` 没有 `Neg`；屏幕 = 内容 − 偏移）。
fn negate(offset: Point) -> Point {
    Point::new(-offset.x, -offset.y)
}

/// 上一帧 lane 里**内容图元**的可见范围（lane 重建 / 隐藏**之前**记下；AR5 P2-1）。
///
/// 文本（`Clip::None` 的长行可以溢出节点盒）与 Canvas 的可见范围都可能大于节点布局盒。
/// 内容**收缩**时如果只按「新图元 + 节点盒」记 damage，旧像素没有任何矩形覆盖 ⇒ 残影
/// （AR5 复现：14 个 'W' 改成 'i'，damage 只到 x=60，旧字形 x∈[61,200] 留在屏幕上）。
/// lane 里恰好保存着上一帧的图元列表，重建前取一份 AABB 即可，不需要额外的常驻状态。
///
/// 两者坐标空间不同（与 `emit_content` 的 damage 写入保持一致）：
/// - 字形是**烘焙坐标**（lane 图元本来就是按 bake 存的）⇒ 加 `lane_offset` 还原成内容坐标，
///   由 `PrimitiveEmitter` 与 `node_damage` 一起统一减祖先滚动偏移转屏幕；
/// - Canvas 是**屏幕坐标**（`screen_baked` 节点，不参与合成端平移）⇒ 直接进 damage。
#[derive(Default)]
struct PaintedContent {
    /// 内容坐标（走 `node_damage` 的记账路径）。
    content: Rect,
    /// 屏幕坐标（直接 push 的 damage）。
    screen: Rect,
}

fn painted_content(tree: &NodeTree, scene: &Scene, node: NodeId) -> PaintedContent {
    let mut content = Rect::default();
    let mut screen = Rect::default();
    // 几何节点可能有多条块 lane（X5）；文本/Canvas 节点恒 ≤1 条。几何图元不进这里
    // （几何的旧可见范围由 `geometry_chunks` 旁路按块精确给出）。
    for index in 0..tree.lane_count(node) {
        let Some(lane) = tree.lane_at(node, index) else {
            continue;
        };
        let Some(primitives) = scene.lane_primitives(lane) else {
            continue;
        };
        for primitive in primitives {
            match primitive {
                Primitive::GlyphRun(run) => {
                    for glyph in &run.glyphs {
                        content = content.union(Rect::from_origin_size(glyph.position, glyph.size));
                    }
                }
                Primitive::Canvas(canvas) => screen = screen.union(canvas.bounds()),
                _ => {}
            }
        }
    }
    PaintedContent {
        content: content.translate(tree.lane_offset(node)),
        screen,
    }
}

impl PrimitiveEmitter {
    fn push(&mut self, context: &mut PaintContext<'_>, primitive: Primitive) {
        let _ = context.scene.push(self.lane, primitive);
        self.emitted += 1;
    }

    /// 累积一块 damage（入参是**烘焙坐标**下的矩形：背景 / 边框 / 渐变的 `bounds` 等）。
    ///
    /// damage 必须留在屏幕坐标（平台 present 的脏矩形、合成线程的 scissor 都是屏幕空间），
    /// 因此这里统一把烘焙坐标平移回屏幕：`screen = bake − 本槽位移`。
    fn push_damage(&self, context: &mut PaintContext<'_>, bounds: Rect, aa_padding: f32) {
        context
            .damage
            .push(bounds.translate(self.damage_shift), aa_padding, self.clip);
    }

    fn emit(&mut self, context: &mut PaintContext<'_>) {
        let visible = self.patch.visible.unwrap_or(true);
        let opacity = match self.patch.opacity {
            Some(value) if value.is_nan() => 0.0,
            Some(value) => value.clamp(0.0, 1.0),
            None => 1.0,
        };
        if !visible || self.bounds.is_empty() {
            self.emit_node_damage(context);
            return;
        }
        self.emit_decorations(context, opacity);
        self.emit_content(context, opacity);
        self.emit_focus_ring(context, opacity);
        self.emit_node_damage(context);
    }

    /// 装饰层（Shadow → 背景/渐变 → 边框）：不参与内容 clip，也不含焦点环/内容。
    ///
    /// 拆成独立方法的唯一原因：几何节点的块 0 lane 要在「装饰之后、焦点环之前」插入几何
    /// 内容（X5 的多 lane 分块），而 `emit` 的单 lane 顺序必须逐图元保持原样。
    fn emit_decorations(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        let spec = self.spec.clone().unwrap_or_default();
        let radii = spec.corner_radius;
        if let Some(shadow) = spec.shadow {
            let color = shadow.color.with_opacity(opacity);
            if !color.is_transparent() {
                // 阴影跟随圆角（R05-9）：`box-shadow` 的轮廓就是 `border-radius` 的轮廓，
                // 半径与背景/边框用同一份 `spec.corner_radius`（编码器再按 CSS 规则 clamp）。
                let primitive = Shadow {
                    rect: self.bounds,
                    offset: shadow.offset,
                    blur: shadow.blur.max(0.0),
                    color,
                    corner_radius: radii,
                };
                self.push(context, Primitive::Shadow(primitive));
                // damage 外扩 = 3σ + 1（σ = blur / 2，物理像素）：与 shader 的顶点外扩一致。
                // 旧的 `blur + 1` 在 σ 改小后会切掉模糊环（可见缺陷）。
                let sigma = (primitive.blur * crate::paint::instance::SHADOW_SIGMA_SCALE)
                    .max(crate::paint::instance::SHADOW_SIGMA_MIN);
                let pad =
                    crate::paint::instance::SHADOW_PAD_SIGMAS * sigma * self.scale.factor() + 1.0;
                self.push_damage(context, primitive.rect.translate(primitive.offset), pad);
            }
        }

        let background = self
            .patch
            .background
            .or(if spec.background.is_transparent() {
                None
            } else {
                Some(spec.background)
            })
            .map(|color| color.with_opacity(opacity));
        let border = self
            .patch
            .border
            .or(spec.border)
            .filter(|border| border.width > 0.0 && !border.color.is_transparent())
            .map(|border| crate::paint::primitive::Border {
                width: border.width,
                color: border.color.with_opacity(opacity),
            });
        let gradient = spec
            .gradient
            .map(|gradient| Gradient {
                from: gradient.from.with_opacity(opacity),
                to: gradient.to.with_opacity(opacity),
                kind: gradient.kind,
            })
            .filter(|gradient| {
                // 两端全透明的渐变不产生像素：不入 lane、不产 damage。
                !(gradient.from.is_transparent() && gradient.to.is_transparent())
            });

        match gradient {
            Some(gradient) => {
                self.push(
                    context,
                    Primitive::Quad(Quad {
                        bounds: self.bounds,
                        corner_radius: radii,
                        paint: QuadPaint::Gradient(gradient),
                    }),
                );
                self.push_damage(context, self.bounds, 1.0);
                if let Some(border) = border {
                    self.push(
                        context,
                        Primitive::Quad(Quad {
                            bounds: self.bounds,
                            corner_radius: radii,
                            paint: QuadPaint::Solid {
                                fill: Color::TRANSPARENT,
                                border: Some(border),
                            },
                        }),
                    );
                }
            }
            None => {
                let fill = background.unwrap_or(Color::TRANSPARENT);
                if !fill.is_transparent() || border.is_some() {
                    self.push(
                        context,
                        Primitive::Quad(Quad {
                            bounds: self.bounds,
                            corner_radius: radii,
                            paint: QuadPaint::Solid { fill, border },
                        }),
                    );
                    self.push_damage(context, self.bounds, 1.0);
                }
            }
        }
    }

    /// 焦点环：在**节点自己的 lane 内**画（受祖先 clip 约束），环在 border box 外侧。
    fn emit_focus_ring(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        let radii = self.spec.clone().unwrap_or_default().corner_radius;
        if let Some(ring) = self.ring
            && ring.width > 0.0
            && !ring.color.with_opacity(opacity).is_transparent()
        {
            let mut radii = radii;
            for radius in &mut radii {
                *radius += ring.offset;
            }
            let bounds = self.bounds.outset(ring.offset);
            self.push(
                context,
                Primitive::Quad(Quad {
                    bounds,
                    corner_radius: radii,
                    paint: QuadPaint::Solid {
                        fill: Color::TRANSPARENT,
                        border: Some(crate::paint::primitive::Border {
                            width: ring.width,
                            color: ring.color.with_opacity(opacity),
                        }),
                    },
                }),
            );
            self.push_damage(context, bounds, 1.0);
        }
    }

    /// 节点级 damage（`touch`/布局写入）转屏幕坐标后并入；装饰/内容/几何都没有产出时也要记
    /// （内容收缩 / 节点不可见时的旧像素）。
    fn emit_node_damage(&mut self, context: &mut PaintContext<'_>) {
        if self.emitted > 0 || !self.node_damage.is_empty() {
            let damage = self.node_damage.translate(negate(self.offset));
            context.damage.push(damage, 1.0, self.clip);
        }
    }

    /// 几何块的内容：内容 clip 括号 + 一条 Primitive::Geometry（X5 的块 lane 内容）。
    ///
    /// chunk 是块号（编码器据此只发该块的 draw range）；paint_geometry_node 已把旧/新块
    /// bounds 的 damage 记好，这里只负责「画什么」。
    fn emit_geometry_chunk(
        &mut self,
        context: &mut PaintContext<'_>,
        id: crate::paint::geometry::GeometryId,
        chunk: u32,
        opacity: f32,
    ) {
        let _ = chunk;
        let content_clip = self.content_clip.map(|rect| ClipRect {
            rect,
            corner_radius: [0.0; 4],
        });
        if let Some(clip_rect) = content_clip {
            let _ = context
                .scene
                .push(self.lane, Primitive::Clip(ClipOp::Push(clip_rect)));
        }
        self.push(
            context,
            Primitive::Geometry(GeometryDraw {
                id,
                transform: self.geometry_transform,
                color: self
                    .patch
                    .foreground
                    .unwrap_or(Color::WHITE)
                    .with_opacity(opacity),
            }),
        );
        if content_clip.is_some() {
            let _ = context.scene.push(self.lane, Primitive::Clip(ClipOp::Pop));
        }
    }

    fn emit_content(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        // 内容按本节点自己的 padding box 裁剪（`Clip::Bounds`）。背景/边框/阴影在
        // `emit` 里先画、不受这个 clip 影响（CSS 的 `overflow` 语义）。
        //
        // 只在**确实有内容**时压括号：`Content::Box`（纯盒子）与还没 shape 好的文本不产生
        // 图元，白压一对 `Clip` 会白占一层 clip 深度（嵌套容器很容易因此提前触顶）。
        let has_content = content_needs_clip(&self.content);
        let content_clip = self
            .content_clip
            .filter(|_| has_content)
            .map(|rect| ClipRect {
                rect,
                // 内容裁剪只按轴对齐矩形裁（与子树 clip 同一形状，圆角留空）。
                corner_radius: [0.0; 4],
            });
        if let Some(clip_rect) = content_clip {
            let _ = context
                .scene
                .push(self.lane, Primitive::Clip(ClipOp::Push(clip_rect)));
        }
        match std::mem::replace(&mut self.content, Content::Box) {
            Content::Box => {}

            Content::Canvas(canvas) => {
                let canvas_bounds = canvas.bounds();
                // E1b F2 + E1d：编码期安全网。`push_shaped_text` 只在 push 时校验租约；push 之后
                // 同帧的图集换代**或逐 tile LRU 淘汰**（不换代）都会让已写入的 tile 变旧 UV。
                // 逐 tile 用 `AtlasAllocator::is_alive` 校验（O(1)，代际 + 槽位分配身份），失效
                // run 不编码（少字优于花字），计数进 `PaintReport`/`FrameRecord`。换代时
                // `App::repaint_text_if_atlas_changed`、淘汰时 `App::layout_text`/`shape_text`
                // 的收尾已把 Canvas 节点标 PAINT，保证本分支在复用路径上也会执行。
                let (canvas, stale_runs) = match context.atlas {
                    Some(atlas) => match canvas.without_stale_glyph_runs(atlas) {
                        Some((filtered, dropped)) => (Arc::new(filtered), dropped),
                        None => (Arc::clone(&canvas), 0),
                    },
                    None => (Arc::clone(&canvas), 0),
                };
                context.stale_canvas_runs += stale_runs;
                self.push(context, Primitive::Canvas(canvas));
                context.damage.push(canvas_bounds, 1.0, self.clip);
            }
            Content::Text(line) => {
                // 不早退：早退会跳过下面的 `Clip(Pop)`，把 clip 泄漏给后续节点。
                if let Some(line) = line {
                    self.emit_text(context, &line, opacity);
                }
            }
        }
        if content_clip.is_some() {
            let _ = context.scene.push(self.lane, Primitive::Clip(ClipOp::Pop));
        }
    }

    fn emit_text(&mut self, context: &mut PaintContext<'_>, line: &ShapedLine, opacity: f32) {
        // D-5：图集代际硬校验（唯一重光栅入口之外的安全网）。`ShapedLine.atlas_generation`
        // 是生成时的图集代际；与当前代际不等说明该行仍烘焙旧 `AtlasTile`（旧 UV）——继续
        // 编码就是整窗花字。本帧跳过该行、记下节点，`App::frame` 提交后重新入队并在下一帧用
        // 当前代际重光栅。`None` = 无 App 的 headless/测试路径，不做校验。
        if let Some(atlas) = context.atlas
            && line.atlas_generation != atlas.generation()
        {
            if !context.stale_text.contains(&self.node) {
                context.stale_text.push(self.node);
            }
            return;
        }
        let origin = Point::new(
            self.bounds.origin.x + self.content_offset.x,
            self.bounds.origin.y + self.content_offset.y,
        );
        for shaped_line in &line.lines {
            for item in &shaped_line.items {
                let ShapedTextItem::GlyphRun(run) = item else {
                    continue;
                };
                if run.glyphs.is_empty() {
                    continue;
                }
                let mut glyphs = Vec::with_capacity(run.glyphs.len());
                let mut glyph_bounds = Rect::default();
                for glyph in &run.glyphs {
                    let position =
                        Point::new(origin.x + glyph.position.x, origin.y + glyph.position.y);
                    let rect = Rect::from_origin_size(position, glyph.size);
                    glyph_bounds = glyph_bounds.union(rect);
                    glyphs.push(PositionedGlyph {
                        tile: glyph.tile,
                        position,
                        size: glyph.size,
                    });
                }
                let color = self
                    .patch
                    .foreground
                    .unwrap_or(run.color)
                    .with_opacity(opacity);
                if color.is_transparent() {
                    continue;
                }
                self.push(context, Primitive::GlyphRun(GlyphRun { color, glyphs }));
                self.push_damage(context, glyph_bounds, 0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PaintPass;
    use crate::{
        math::{PhysicalRect, Point, Rect, Scale},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::{
            canvas::Canvas,
            color::Color,
            geometry::GeometryId,
            primitive::{Border, ClipOp, Gradient, GradientKind, Primitive, QuadPaint},
            scene::{LaneId, Scene},
            style::{Clip, PaintSpec, ShadowSpec, Style, StylePatch},
        },
        render::damage::DamageAccumulator,
    };

    fn scale() -> Scale {
        Scale::new(1.0).expect("合法 scale")
    }

    fn frame() -> PhysicalRect {
        PhysicalRect::new(0, 0, 400, 300)
    }

    struct Harness {
        tree: NodeTree,
        scene: Scene,
        damage: DamageAccumulator,
        order: Vec<LaneId>,
        scroll_slots: Vec<(Point, PhysicalRect)>,
        scroll_slot_nodes: Vec<crate::node::id::NodeId>,
        /// D-5 / E1d：绘制前图集校验的注入点（None = 不校验；Some = 编码期逐 tile 用
        /// `AtlasAllocator::is_alive` 判活）。
        atlas: Option<crate::render::AtlasAllocator>,
        /// 几何注册表（几何节点的块 lane 测试用；X5）。
        geometry: crate::paint::geometry_store::GeometryStore,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                tree: NodeTree::new(),
                scene: Scene::empty(),
                damage: DamageAccumulator::new(scale()),
                order: Vec::new(),
                scroll_slots: Vec::new(),
                scroll_slot_nodes: Vec::new(),
                atlas: None,
                geometry: crate::paint::geometry_store::GeometryStore::new(),
            }
        }

        fn try_paint(&mut self) -> Result<super::PaintReport, crate::error::EngineError> {
            self.damage.reset(scale());
            self.order.clear();
            let geometry = &self.geometry;
            let mut pass = PaintPass {
                tree: &mut self.tree,
                geometry,
                scene: &mut self.scene,
                damage: &mut self.damage,
                scale: scale(),
                frame: frame(),
                order: &mut self.order,
                focused: None,
                atlas: self.atlas.as_ref(),
                scroll_slots: &mut self.scroll_slots,
                scroll_slot_nodes: &mut self.scroll_slot_nodes,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            };
            pass.run()
        }

        fn paint(&mut self) -> super::PaintReport {
            self.try_paint().expect("paint")
        }

        /// 增量路径：调用方先把脏节点标进闭包（`mark_incremental`），与 `Window::paint_frame`
        /// 的两步（分类后 `mark_paint_closure` + `run_incremental`）一致。
        fn try_paint_incremental(
            &mut self,
            dirty: &[crate::node::id::NodeId],
        ) -> Result<super::PaintReport, crate::error::EngineError> {
            self.damage.reset(scale());
            self.tree.mark_paint_closure(dirty);
            let geometry = &self.geometry;
            let mut pass = PaintPass {
                tree: &mut self.tree,
                geometry,
                scene: &mut self.scene,
                damage: &mut self.damage,
                scale: scale(),
                frame: frame(),
                order: &mut self.order,
                focused: None,
                atlas: self.atlas.as_ref(),
                scroll_slots: &mut self.scroll_slots,
                scroll_slot_nodes: &mut self.scroll_slot_nodes,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            };
            pass.run_incremental()
        }

        fn paint_incremental(&mut self, dirty: &[crate::node::id::NodeId]) -> super::PaintReport {
            self.try_paint_incremental(dirty)
                .expect("incremental paint")
        }

        fn mark_paint(&mut self, node: crate::node::id::NodeId) {
            let _ = self.tree.mark_paint(node);
        }

        /// 标脏并返回（方便一次标多个节点后走增量路径）。
        fn mark_dirty(&mut self, nodes: &[crate::node::id::NodeId]) {
            for node in nodes {
                let _ = self.tree.mark_paint(*node);
            }
        }
    }

    fn fixed_style(width: f32, height: f32) -> Style {
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

    fn with_white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    /// 给每个节点写一个固定的绝对布局盒（paint 只读 `layout`，不需要 taffy）。
    fn layout_all(tree: &mut NodeTree) {
        let order: Vec<_> = tree.dfs_ids();
        for (index, id) in order.iter().enumerate() {
            let bounds = Rect::new(0.0, (index as f32) * 20.0, 100.0, 20.0);
            let layout = crate::node::tree::LayoutBox {
                bounds,
                border: crate::math::Edges::ZERO,
            };
            let _ = tree.set_layout_for_test(*id, layout);
        }
    }

    fn lane_primitives(harness: &Harness, node: crate::node::id::NodeId) -> Vec<Primitive> {
        let lane = harness.tree.lane_of(node).expect("lane");
        harness.scene.lane_primitives(lane).expect("存在").to_vec()
    }

    /// **滚动合成端偏移的接线**（复审 AR4 P1-3）：滚动容器拿到稳定槽位，且**自己的**偏移不进
    /// 图元烘焙坐标 ⇒ 滚动容器内的内容滚动时一条 lane 都不重建、图元字节一个都不动。
    ///
    /// 变异守卫：把 `paint_node` 里「子树的 `slot_offset` 换成容器自己的偏移」改错（例如沿用
    /// 祖先的 `slot_offset`），本用例会看到 `rebuilt > 0` 且图元位置变了。
    #[test]
    fn scrolling_only_moves_the_slot_offset_not_the_lane_bytes() {
        use crate::{
            interaction::{ScrollAxis, ScrollSlot},
            math::Size,
        };

        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 100.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        // 与 `App::make_scroll` 写的是同一份状态（`Node.interaction.scroll`）。
        h.tree
            .ensure_interaction(container)
            .expect("interaction")
            .scroll = Some(ScrollSlot {
            offset: Point::ZERO,
            content: Size::new(100.0, 400.0),
            axis: ScrollAxis::Vertical,
            line_height: 48.0,
        });
        h.tree.note_scroll_container(container);

        let first = h.paint();
        assert!(first.rebuilt >= 2, "首帧两条 lane 都要建");
        assert_eq!(h.scroll_slots.len(), 2, "槽位 0 是哨兵，滚动容器占 1");
        assert_eq!(h.scroll_slots[1].0, Point::ZERO);
        // 视口 = 容器的 padding box（屏幕坐标，物理化；夹具无 border）。
        let container_box = h.tree.get(container).expect("container").layout().bounds;
        assert_eq!(
            h.scroll_slots[1].1,
            PhysicalRect::new(
                container_box.origin.x as i32,
                container_box.origin.y as i32,
                (container_box.origin.x + container_box.size.width) as i32,
                (container_box.origin.y + container_box.size.height) as i32,
            ),
            "视口 = 容器的 padding box（物理像素）"
        );
        let before = lane_primitives(&h, child);
        assert!(
            !before
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Clip(_))),
            "滚动容器的视口 clip 不得烘焙进 lane：它在内容坐标里随滚动移动，烘焙进去就要求\
             每滚一像素重编码全部 lane（改由槽位的屏幕裁剪区做每 op scissor）"
        );
        // 内容烘焙在**容器内容空间**：不含容器自己的滚动位移。
        let quad_bounds = |primitives: &[Primitive]| {
            primitives
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Quad(quad) => Some(quad.bounds),
                    _ => None,
                })
                .expect("child 有背景 quad")
        };
        assert_eq!(
            quad_bounds(&before),
            h.tree.get(child).expect("child").layout().bounds,
            "图元烘焙在容器内容空间（= 布局坐标，不含容器自己的滚动位移）"
        );

        // 滚 30 px：**不**标脏任何节点，只改滚动槽（生产路径由 `App::scroll_to` 写偏移 + 视口
        // damage；paint pass 因为有待画 damage 而重跑）。
        let lane = h.tree.lane_of(child).expect("child lane");
        let version = h.scene.lane_state(lane).expect("lane").0;
        h.tree.scroll_slot_mut(container).expect("slot").offset = Point::new(0.0, 30.0);
        let second = h.paint();
        assert_eq!(second.rebuilt, 0, "滚动不得重建任何 lane");
        assert_eq!(h.scroll_slots[1].0, Point::new(0.0, 30.0), "位移进槽位表");
        assert_eq!(
            lane_primitives(&h, child),
            before,
            "滚动不得改图元字节（偏移在绘制期由 GPU 加上）"
        );
        assert_eq!(
            h.scene.lane_state(lane).expect("lane").0,
            version,
            "图元没变 ⇒ 内容版本不得前进（否则合成端会重编码）"
        );
        assert_eq!(
            h.scene.lane_scroll(lane),
            1,
            "lane 必须登记滚动槽 1（编码器据此生成 FrameOp::Instances.scroll）"
        );
    }

    /// X5：块 lane 的 damage = `transform(块 AABB)` 减祖先滚动累计值（逻辑 → 屏幕坐标）。
    #[test]
    fn geometry_chunk_damage_uses_the_chunk_aabb_and_transform() {
        let chunk = Rect::new(0.0, 0.0, 200.0, 200.0);
        let damage =
            super::geometry_chunk_damage(crate::math::Transform::IDENTITY, chunk, Point::ZERO);
        assert_eq!(damage, chunk, "无变换 / 无滚动时块 AABB 原样");

        let scrolled = super::geometry_chunk_damage(
            crate::math::Transform::IDENTITY,
            chunk,
            Point::new(0.0, 50.0),
        );
        assert_eq!(
            scrolled,
            Rect::new(0.0, -50.0, 200.0, 200.0),
            "祖先滚动累计值要换算到屏幕坐标"
        );

        // R05-12：per-draw transform 必须进入 damage 范围（否则平移后残影）。
        let moved = super::geometry_chunk_damage(
            crate::math::Transform::translate(30.0, 40.0),
            chunk,
            Point::ZERO,
        );
        assert_eq!(moved, Rect::new(30.0, 40.0, 200.0, 200.0));

        // 空块不产生 damage。
        assert!(
            super::geometry_chunk_damage(
                crate::math::Transform::IDENTITY,
                Rect::default(),
                Point::ZERO
            )
            .is_empty()
        );
    }

    #[test]
    fn background_and_border_fit_one_quad() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                border: Some(Border {
                    width: 1.0,
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let report = h.paint();
        assert_eq!(report.rebuilt, 1, "只有真正产生像素的节点计入重建");
        assert!(
            report.damage.rects > 0 || report.damage.full,
            "产生像素的节点必须累积 damage"
        );
        let primitives = lane_primitives(&h, node);
        assert_eq!(primitives.len(), 1);
        match &primitives[0] {
            Primitive::Quad(quad) => match &quad.paint {
                QuadPaint::Solid { fill, border } => {
                    assert!(!fill.is_transparent());
                    assert!(border.is_some());
                }
                QuadPaint::Gradient(_) => panic!("应为实色"),
            },
            _ => panic!("应为 Quad"),
        }
    }

    #[test]
    fn gradient_plus_border_is_two_quads() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                gradient: Some(Gradient {
                    from: Color::WHITE,
                    to: Color::BLACK,
                    kind: GradientKind::Horizontal,
                }),
                border: Some(Border {
                    width: 1.0,
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let primitives = lane_primitives(&h, node);
        assert_eq!(primitives.len(), 2);
        assert!(matches!(primitives[0], Primitive::Quad(_)));
        assert!(matches!(primitives[1], Primitive::Quad(_)));
    }

    #[test]
    fn shadow_precedes_fill() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                shadow: Some(ShadowSpec {
                    offset: Point::new(1.0, 2.0),
                    blur: 3.0,
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let primitives = lane_primitives(&h, node);
        assert!(matches!(primitives[0], Primitive::Shadow(_)));
        assert!(matches!(primitives[1], Primitive::Quad(_)));
    }

    #[test]
    fn hidden_node_paints_nothing_and_clears_lane() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(!h.scene.is_empty());

        let patch = StylePatch {
            visible: Some(false),
            ..StylePatch::default()
        };
        let _ = h.tree.set_style_patch_for_test(node, patch);
        h.mark_paint(node);
        let _ = h.paint();
        // 与 `display: none` 同一条 `hide_subtree` 路径：lane 保留分配但被清空，
        // 因此断言「没有图元」而不是「没有 lane」（后者是旧实现 rebuild_lane 的副作用）。
        assert!(
            h.tree
                .lane_of(node)
                .and_then(|lane| h.scene.lane_primitives(lane))
                .is_none_or(<[Primitive]>::is_empty),
            "隐藏节点不得留下任何图元"
        );
        assert!(h.scene.is_empty(), "隐藏之后场景里不应还有任何内容");
        assert!(!h.damage.finish(frame()).full);
    }

    /// P2-3.2 回归：`visible: false` 隐藏**整棵子树**，与命中测试/焦点遍历/a11y 同一语义。
    ///
    /// 旧实现只跳过本节点自身的图元，子控件仍然被画出来 —— 而它们在命中测试里是不可见的，
    /// 于是出现「看得见但点不动」的死像素。
    #[test]
    fn hiding_a_container_hides_its_children() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载容器");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("挂载子节点");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(!h.scene.is_empty(), "初始状态两者都画出来");

        let _ = h.tree.set_style_patch_for_test(
            container,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        h.mark_paint(container);
        let _ = h.paint();
        assert!(
            h.scene.is_empty(),
            "容器隐藏后子控件也必须消失（旧的『只隐藏自身』会留下子控件）"
        );
        let _ = child;
    }
    /// P2-3.2 回归：`Clip::Bounds` 必须裁剪节点**自身的内容**。
    ///
    /// 旧实现只把 clip 压给子树，于是「容器剪不住自己的 Canvas/文本」：图表的柱子会溢出
    /// 到容器外。CSS 语义里 `overflow` 裁的是内容，装饰（背景/边框）不受影响。
    #[test]
    fn own_clip_brackets_the_node_content() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(60.0, 20.0);
        style.clip = Clip::Bounds;
        // 用 Canvas 作内容：paint harness 不做文本 shape，文本内容为空不会产生图元。
        let mut canvas = crate::paint::canvas::Canvas::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        canvas.push_quad(crate::paint::primitive::Quad {
            bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: crate::paint::color::Color::WHITE,
                border: None,
            },
        });
        let clipped = NodeBuilder::canvas(canvas)
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        // 对照：同样形状但**不**裁剪的 Canvas 节点不应有额外括号。
        let mut plain_canvas = crate::paint::canvas::Canvas::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        plain_canvas.push_quad(crate::paint::primitive::Quad {
            bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: crate::paint::color::Color::WHITE,
                border: None,
            },
        });
        let plain = NodeBuilder::canvas(plain_canvas)
            .style(fixed_style(60.0, 20.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();

        let primitives = lane_primitives(&h, clipped);
        assert!(
            matches!(primitives.first(), Some(Primitive::Clip(ClipOp::Push(_)))),
            "内容前必须有自身 padding box 的 clip：{primitives:?}"
        );
        assert!(
            matches!(primitives.last(), Some(Primitive::Clip(ClipOp::Pop))),
            "内容 clip 必须闭合：{primitives:?}"
        );
        assert!(
            primitives
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Canvas(_))),
            "Canvas 内容必须在括号内"
        );
        let plain_primitives = lane_primitives(&h, plain);
        assert!(
            !plain_primitives
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Clip(_))),
            "不裁剪的节点不得额外压 clip：{plain_primitives:?}"
        );
    }

    #[test]
    fn clip_brackets_lane_and_subtree() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, clipped)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(matches!(h.tree.clip_key(child), Some((1, Some(_)))));
        let primitives = lane_primitives(&h, child);
        assert!(matches!(primitives[0], Primitive::Clip(ClipOp::Push(_))));
        assert!(matches!(
            primitives.last(),
            Some(Primitive::Clip(ClipOp::Pop))
        ));
    }

    #[test]
    fn lane_is_reused_when_reusable() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let before = lane_primitives(&h, node);
        let report = h.paint();
        assert_eq!(report.rebuilt, 0, "全部可复用");
        assert_eq!(lane_primitives(&h, node), before);
    }

    #[test]
    fn canvas_and_geometry_emit_primitives() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 10.0, 10.0)))
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        let geometry = NodeBuilder::geometry(GeometryId::for_test(0, 1))
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(matches!(
            lane_primitives(&h, canvas).first(),
            Some(Primitive::Canvas(_))
        ));
        assert!(matches!(
            lane_primitives(&h, geometry).first(),
            Some(Primitive::Geometry(_))
        ));
    }

    #[test]
    fn geometry_node_transform_reaches_the_primitive() {
        // R05-12：节点级几何的 per-draw 变换必须进 `Primitive::Geometry`（旧实现硬编码
        // `Transform::IDENTITY`，图表平移只能重建顶点）。顶点缓冲不动，只换 uniform。
        let mut h = Harness::new();
        let root = h.tree.root();
        let geometry = NodeBuilder::geometry(GeometryId::for_test(0, 1))
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        h.tree
            .set_geometry_transform(
                geometry,
                crate::math::Transform::translate(4.0, 6.0),
                crate::math::Rect::default(),
            )
            .expect("几何节点");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let Some(Primitive::Geometry(draw)) = lane_primitives(&h, geometry).first().cloned() else {
            panic!("必须产生一条几何绘制");
        };
        assert_eq!(
            draw.transform,
            crate::math::Transform::translate(4.0, 6.0),
            "节点级几何的 transform 必须是 setter 写入的那个"
        );
        // 非几何节点：setter 必须报 StaleNode（不能静默写到错误节点上）。
        let boxed = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        assert!(matches!(
            h.tree.set_geometry_transform(
                boxed,
                crate::math::Transform::IDENTITY,
                crate::math::Rect::default()
            ),
            Err(crate::error::EngineError::StaleNode { .. })
        ));
    }

    #[test]
    fn clip_key_mismatch_after_resize_or_reparent() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, clipped)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let before = lane_primitives(&h, child);
        let first_rect = match &before[0] {
            Primitive::Clip(ClipOp::Push(clip)) => clip.rect,
            _ => panic!("应以 clip push 开始"),
        };

        // resize：只改 clip 容器的布局盒，子节点绝对盒不变。
        let resize = crate::node::tree::LayoutBox {
            bounds: Rect::new(0.0, 0.0, 60.0, 60.0),
            border: crate::math::Edges::ZERO,
        };
        let _ = h.tree.set_layout_for_test(clipped, resize);
        let report = h.paint();
        assert!(report.rebuilt > 0, "clip 容器尺寸变化必须触发重建");
        let after = lane_primitives(&h, child);
        let second_rect = match &after[0] {
            Primitive::Clip(ClipOp::Push(clip)) => clip.rect,
            _ => panic!("应以 clip push 开始"),
        };
        assert_ne!(first_rect, second_rect, "前缀矩形必须随 clip 容器更新");
        assert_eq!(
            h.tree.clip_key(child),
            Some((1, Some(second_rect.to_physical_covering(scale()))))
        );
    }

    #[test]
    fn reparent_under_clip_forces_rebuild() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert_eq!(h.tree.clip_key(child), Some((0, None)));

        h.tree.append_child(clipped, child).expect("重挂");
        let _ = h.paint();
        let primitives = lane_primitives(&h, child);
        assert!(matches!(primitives[0], Primitive::Clip(ClipOp::Push(_))));
        assert!(matches!(h.tree.clip_key(child), Some((1, Some(_)))));
    }

    #[test]
    fn zero_size_clip_still_pushes_and_empties_damage() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, clipped)
            .expect("挂载");
        layout_all(&mut h.tree);
        // 0×0 的 clip 容器：padding box 为空，但仍必须入栈（B1）。
        let empty = crate::node::tree::LayoutBox {
            bounds: Rect::default(),
            border: crate::math::Edges::ZERO,
        };
        let _ = h.tree.set_layout_for_test(clipped, empty);
        let report = h.paint();

        let primitives = lane_primitives(&h, child);
        match &primitives[0] {
            Primitive::Clip(ClipOp::Push(clip)) => {
                assert!(clip.rect.is_empty(), "空 padding box 必须原样入栈");
            }
            _ => panic!("子 lane 必须以 ClipPush 开始（即使 rect 为空）"),
        }
        assert!(
            h.tree.clip_key(child).is_some_and(|(depth, _)| depth == 1),
            "空 clip 也要计入深度"
        );
        assert_eq!(report.damage.rects, 0, "空 clip 内的 damage 必须被裁空");
        assert!(!report.damage.full);
    }

    #[test]
    fn transparent_gradient_emits_nothing() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let _node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                gradient: Some(Gradient {
                    from: Color::TRANSPARENT,
                    to: Color::TRANSPARENT,
                    kind: GradientKind::Vertical,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let report = h.paint();
        assert_eq!(report.rebuilt, 0, "全透明渐变不产生像素");
        assert_eq!(report.damage.rects, 0);
    }

    /// 叠 `depth` 层 `Clip::Bounds`，返回最内层节点。
    fn nest_clips(h: &mut Harness, depth: usize) -> crate::node::id::NodeId {
        let root = h.tree.root();
        let mut parent = root;
        for _ in 0..depth {
            let mut style = fixed_style(100.0, 100.0);
            style.clip = Clip::Bounds;
            parent = NodeBuilder::boxed()
                .style(style)
                .build(&mut h.tree, parent)
                .expect("挂载");
        }
        layout_all(&mut h.tree);
        parent
    }

    #[test]
    fn clip_depth_at_limit_is_allowed() {
        let mut h = Harness::new();
        let deepest = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH));
        // 最内层节点的祖先链 = 7 层（第 8 层是它自己的子树），允许。
        let _ = h.paint();
        assert!(
            h.tree
                .clip_key(deepest)
                .is_some_and(|(depth, _)| depth == 7)
        );
    }

    #[test]
    fn empty_clipping_containers_do_not_consume_clip_depth() {
        // 纯 `Box` 容器不产图元 ⇒ 也不压内容 clip。旧实现按「有 `Clip::Bounds` 就算一层」
        // 校验深度，8 层嵌套的空容器会被误判超深（`ClipDepthExceeded`），实际它一个图元
        // 都没有、根本不需要 clip。
        let mut h = Harness::new();
        let _ = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH));
        // `h.paint()` 内部会 `expect` 成功：这里唯一要验的就是**不报** `ClipDepthExceeded`。
        // 各层 lane 里仍有「祖先 clip 的括号」原始图元，所以不能断言 rebuilt == 0。
        let _ = h.paint();
    }

    /// P2-7 回归：超过 `MAX_CLIP_DEPTH` 的嵌套裁剪容器**折叠**而不是失败。
    ///
    /// 旧实现在这里返回 `ClipDepthExceeded` 让整帧中止 —— 8 层嵌套的滚动容器（深层
    /// 面板 + 列表 + 弹层）会直接白屏。矩形 clip 的复合就是交集，求交是无损的，因此
    /// 折叠既保住精度也不牺牲可用性。
    #[test]
    fn clip_depth_over_limit_folds_instead_of_failing() {
        let mut h = Harness::new();
        let deepest = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH) + 1);
        let report = h.paint();
        // 折叠后每一层都还在（线段里仍有 clip 图元），关键是没有报错、没有丢绘制。
        assert!(report.rebuilt > 0, "折叠后必须仍能绘制");
        // 最深层带内容时也要能画：这里只验证 clip 上下文结构（`clip_key` 深度被折叠到上限）。
        assert!(
            h.tree
                .clip_key(deepest)
                .is_some_and(|(depth, _)| usize::from(depth)
                    <= usize::from(crate::paint::scene::MAX_CLIP_DEPTH)),
            "折叠后每层的 clip 深度不得超过上限"
        );
    }

    // ── A1：增量路径（脏闭包）单元测试 ─────────────────────────────────────

    /// 单节点 lane 的内容版本（增量路径「未访问节点不得被触碰」的判据）。
    fn lane_version(h: &Harness, lane: LaneId) -> u32 {
        h.scene.lane_state(lane).expect("lane 活着").0
    }

    /// 场景的「绘制相关」快照：按 DFS 的（节点, lane 图元, 滚动槽）+ 仅含非空 lane 的绘制序 +
    /// 滚动槽表。用于增量 vs 全量的逐字段等价断言（空 lane 是否在绘制序里不影响像素）。
    fn rendered_state(
        h: &Harness,
    ) -> (
        Vec<(NodeId, Vec<Primitive>, u32)>,
        Vec<NodeId>,
        Vec<(Point, PhysicalRect)>,
    ) {
        let ids = h.tree.dfs_ids();
        let mut lanes = Vec::new();
        let mut lane_nodes = ahash::HashMap::default();
        for id in ids {
            let Some(lane) = h.tree.lane_of(id) else {
                continue;
            };
            lane_nodes.insert(lane, id);
            let primitives = h
                .scene
                .lane_primitives(lane)
                .map_or_else(Vec::new, <[Primitive]>::to_vec);
            lanes.push((id, primitives, h.scene.lane_scroll(lane)));
        }
        let drawn_order: Vec<NodeId> = h
            .order
            .iter()
            .filter(|lane| {
                h.scene
                    .lane_primitives(**lane)
                    .is_some_and(|primitives| !primitives.is_empty())
            })
            .filter_map(|lane| lane_nodes.get(lane).copied())
            .collect();
        (lanes, drawn_order, h.scroll_slots.clone())
    }

    /// A1 / P1A-1a：增量帧只访问脏闭包（变化节点 + 祖先链），兄弟子树整棵跳过；绘制序
    /// 与未访问 lane 完全不动（这是「不重建 order ⇒ `delta.order == None`」的单元级证据）。
    #[test]
    fn incremental_only_visits_the_dirty_closure() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(200.0, 100.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let left = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 40.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("left");
        let right = NodeBuilder::boxed()
            .layout_override(Rect::new(60.0, 0.0, 40.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("right");
        let sibling = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 40.0, 40.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("sibling");
        layout_all(&mut h.tree);

        let first = h.paint();
        assert_eq!(first.rebuilt, 4, "四个有图元的节点首帧都要建 lane");
        let order_before = h.order.clone();
        let left_lane = h.tree.lane_of(left).expect("left lane");
        let right_lane = h.tree.lane_of(right).expect("right lane");
        let sibling_lane = h.tree.lane_of(sibling).expect("sibling lane");
        let (left_v, right_v, sibling_v) = (
            lane_version(&h, left_lane),
            lane_version(&h, right_lane),
            lane_version(&h, sibling_lane),
        );
        let counters_before = h.tree.paint_counters();

        h.mark_paint(left);
        let report = h.paint_incremental(&[left]);
        assert_eq!(report.rebuilt, 1, "只有变化节点重建 lane");
        let counters = h.tree.paint_counters();
        assert_eq!(
            counters.visited_nodes, 3,
            "访问数 = left + container + root"
        );
        assert!(
            counters.visited_nodes < h.tree.len(),
            "访问数必须小于树规模（真实负载下远小于）：{} vs {}",
            counters.visited_nodes,
            h.tree.len()
        );
        assert_eq!(
            counters.full_frames, counters_before.full_frames,
            "增量帧不得退化为全量"
        );
        assert_eq!(
            counters.incremental_frames,
            counters_before.incremental_frames + 1
        );
        assert_eq!(
            counters.order_rebuilds, counters_before.order_rebuilds,
            "没有 lane 成员变化 ⇒ 不得重建绘制序"
        );
        assert_eq!(h.order, order_before, "增量路径不得改绘制序");
        assert_eq!(h.scene.order(), order_before.as_slice());
        assert!(
            lane_version(&h, left_lane) > left_v,
            "被改 lane 的内容版本必须推进"
        );
        assert_eq!(
            lane_version(&h, right_lane),
            right_v,
            "兄弟 lane 不得被触碰"
        );
        assert_eq!(
            lane_version(&h, sibling_lane),
            sibling_v,
            "无关子树 lane 不得被触碰"
        );
        #[cfg(feature = "test-support")]
        {
            let log = &report.log.lanes;
            assert!(log.iter().any(|entry| entry.node == left));
            assert!(
                !log.iter().any(|entry| entry.node == right),
                "未变化的兄弟不得被访问：{log:?}"
            );
            assert!(
                !log.iter().any(|entry| entry.node == sibling),
                "无关子树不得被访问：{log:?}"
            );
        }
    }

    /// A1：clip 上下文变化会让整棵子树失配 ⇒ 增量路径必须保守展开子树（全量路径的语义）。
    #[test]
    fn incremental_expands_the_subtree_when_the_clip_context_changes() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(100.0, 100.0))
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let before = lane_primitives(&h, child);
        assert!(
            !before
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Clip(_))),
            "初始没有 clip 前缀"
        );

        let mut clipped = fixed_style(100.0, 100.0);
        clipped.clip = Clip::Bounds;
        h.tree.set_style_for_test(container, clipped);
        h.mark_paint(container);
        let report = h.paint_incremental(&[container]);
        assert!(report.rebuilt >= 1, "clip 变化必须重建子树 lane");
        let after = lane_primitives(&h, child);
        assert!(
            matches!(after.first(), Some(Primitive::Clip(ClipOp::Push(_)))),
            "clip 变化必须传播到子树：{after:?}"
        );
        assert!(
            h.tree.clip_key(child).is_some_and(|(depth, _)| depth == 1),
            "子树的 clip_key 必须写回新上下文"
        );
    }

    /// A1：隐藏变化必须访问整棵子树（`hide_subtree`），清空它们的 lane 并产生 damage；
    /// 但**不**改变绘制序成员（lane 保留分配 ⇒ 再次显示时位置稳定）。
    #[test]
    fn incremental_hides_the_whole_subtree() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(100.0, 100.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(!h.scene.is_empty());
        let container_lane = h.tree.lane_of(container).expect("lane");
        let child_lane = h.tree.lane_of(child).expect("lane");
        let order_before = h.order.clone();

        h.tree.set_style_patch_for_test(
            container,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        h.mark_paint(container);
        let report = h.paint_incremental(&[container]);
        assert!(report.rebuilt >= 2, "整棵子树都要清空");
        assert!(report.damage.rects > 0, "隐藏必须产生重绘 damage");
        assert!(h.scene.is_empty(), "隐藏之后场景里不应还有任何内容");
        assert!(
            h.scene
                .lane_primitives(container_lane)
                .is_some_and(<[Primitive]>::is_empty),
            "容器 lane 必须被清空（保留分配）"
        );
        assert!(
            h.scene
                .lane_primitives(child_lane)
                .is_some_and(<[Primitive]>::is_empty),
            "子节点 lane 也必须被清空（hide_subtree 递归）"
        );
        assert_eq!(h.order, order_before, "隐藏不改变绘制序成员");
    }

    /// A1：增量路径出现「新建 lane」⇒ 绘制序成员变化 ⇒ 用轻量 DFS 兜底重建绘制序。
    #[test]
    fn incremental_new_lane_rebuilds_the_order() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("node");
        layout_all(&mut h.tree);
        // 先让它隐藏地画一帧：隐藏子树不分配 lane。
        h.tree.set_style_patch_for_test(
            node,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        let _ = h.paint();
        assert!(
            h.tree.lane_of(node).is_none(),
            "隐藏节点不得在首帧分配 lane"
        );
        assert!(h.order.is_empty(), "隐藏节点不进绘制序");

        // 重新显示：重建时新建 lane ⇒ 顺序兜底必须把它放进绘制序。
        h.tree.set_style_patch_for_test(
            node,
            StylePatch {
                visible: Some(true),
                ..StylePatch::default()
            },
        );
        h.mark_paint(node);
        let counters_before = h.tree.paint_counters();
        let report = h.paint_incremental(&[node]);
        assert_eq!(report.rebuilt, 1);
        let lane = h.tree.lane_of(node).expect("重新显示必须新建 lane");
        assert_eq!(h.order, vec![lane], "兜底重建的绘制序必须包含新 lane");
        assert_eq!(h.scene.order(), h.order.as_slice());
        assert_eq!(
            h.tree.paint_counters().order_rebuilds,
            counters_before.order_rebuilds + 1,
            "新建 lane 必须记一次绘制序兜底"
        );
    }

    /// A1：增量帧访问滚动容器时用**缓存槽号**写偏移，不重新稠密编号（已烘焙进 lane 的
    /// 槽号不能动）。
    #[test]
    fn incremental_reuses_the_cached_scroll_slot() {
        use crate::{
            interaction::{ScrollAxis, ScrollSlot},
            math::Size,
        };

        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 100.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        h.tree
            .ensure_interaction(container)
            .expect("interaction")
            .scroll = Some(ScrollSlot {
            offset: Point::ZERO,
            content: Size::new(100.0, 400.0),
            axis: ScrollAxis::Vertical,
            line_height: 48.0,
        });
        h.tree.note_scroll_container(container);
        let first = h.paint();
        assert!(first.rebuilt >= 2);
        assert_eq!(h.scroll_slots.len(), 2, "哨兵 + 一个滚动槽");
        let child_lane = h.tree.lane_of(child).expect("child lane");
        assert_eq!(h.scene.lane_scroll(child_lane), 1, "lane 登记槽位 1");

        // 滚动 + 子树内容变化同一帧：走增量路径（有 paint 工作，用不上纯滚动快路径）。
        h.tree.scroll_slot_mut(container).expect("slot").offset = Point::new(0.0, 30.0);
        h.mark_paint(child);
        let report = h.paint_incremental(&[child]);
        assert_eq!(report.rebuilt, 1, "只有脏节点重建");
        assert_eq!(h.scroll_slots.len(), 2, "增量路径不得重排 / 追加槽位");
        assert_eq!(
            h.scroll_slots[1].0,
            Point::new(0.0, 30.0),
            "偏移必须写回缓存的槽位"
        );
        assert_eq!(h.scene.lane_scroll(child_lane), 1, "槽号不得改变");
    }

    /// A1 / P1A-1b 的单元级差分 oracle：同一状态下，增量路径与全量路径产出的
    /// lane 内容 / 每 lane 滚动槽 / 绘制序（按绘制内容归一化）/ 滚动槽表逐字段一致。
    #[test]
    fn incremental_and_full_paths_produce_the_same_scene() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut clip_style = fixed_style(100.0, 100.0);
        clip_style.clip = Clip::Bounds;
        let clip_container = NodeBuilder::boxed()
            .style(clip_style)
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("clip container");
        let inner = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, clip_container)
            .expect("inner");
        let _sibling = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 50.0, 40.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("sibling");
        let hider = NodeBuilder::boxed()
            .layout_override(Rect::new(60.0, 50.0, 40.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("hider");
        layout_all(&mut h.tree);
        let mut first = h.paint();
        assert!(first.rebuilt >= 4);

        // 一串增量修改：clip 容器改尺寸（clip 上下文 + damage）、隐藏 hider、inner 重建。
        let resize = crate::node::tree::LayoutBox {
            bounds: Rect::new(0.0, 0.0, 60.0, 60.0),
            border: crate::math::Edges::ZERO,
        };
        let _ = h.tree.set_layout_for_test(clip_container, resize);
        h.tree.set_style_patch_for_test(
            hider,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        h.mark_dirty(&[clip_container, hider, inner]);
        let _ = h.paint_incremental(&[clip_container, hider, inner]);
        let incremental = rendered_state(&h);

        // 同一状态走全量路径（所有节点标脏 ⇒ 全量重放）。
        let ids = h.tree.dfs_ids();
        h.mark_dirty(&ids);
        first = h.paint();
        assert!(first.rebuilt >= 4, "全量重放必须重建全部有图元的节点");
        let full = rendered_state(&h);

        assert_eq!(
            incremental.0.len(),
            full.0.len(),
            "lane 集合（按节点归一化）必须一致"
        );
        assert_eq!(incremental, full, "增量与全量产物必须逐字段一致");
    }
    // ── X5：几何节点多 lane 分块 ─────────────────────────────────────────────

    /// `triangles` 个互不共享顶点的三角形（真实位置；每 4096 个三角形一块）。
    fn long_geometry(triangles: u32) -> crate::paint::geometry::GeometryDesc {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};
        let mut vertex_bytes = Vec::with_capacity(triangles as usize * 36);
        let mut index_bytes = Vec::with_capacity(triangles as usize * 6);
        for triangle in 0..triangles {
            let base = (triangle * 3) as u16;
            let x = (triangle % 64) as f32;
            let y = (triangle / 64) as f32;
            for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
                vertex_bytes.extend_from_slice(&(x + dx).to_ne_bytes());
                vertex_bytes.extend_from_slice(&(y + dy).to_ne_bytes());
                vertex_bytes.extend_from_slice(&(-1.0f32).to_ne_bytes());
            }
            for index in [base, base + 1, base + 2] {
                index_bytes.extend_from_slice(&index.to_ne_bytes());
            }
        }
        GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(index_bytes),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 64.0, 100.0),
            pipeline: GeometryPipeline::Ui,
        }
    }

    /// 挂一个几何节点并把布局盒写成固定值（paint 只读 `layout`）。
    fn mount_geometry(h: &mut Harness, id: GeometryId, clip: Clip) -> NodeId {
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = clip;
        let node = NodeBuilder::geometry(id)
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载几何节点");
        layout_all(&mut h.tree);
        node
    }

    /// 在几何末尾追加一个三角形（顶点 range + 索引 range 两次更新，与 K 线追加同路径）。
    fn append_triangle(h: &mut Harness, id: GeometryId, x: f32, y: f32) {
        let snapshot = h.geometry.snapshot_arc(id).expect("已注册");
        let vertex_at = snapshot.vertex_len();
        let mut vertices = Vec::with_capacity(36);
        for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
            vertices.extend_from_slice(&(x + dx).to_ne_bytes());
            vertices.extend_from_slice(&(y + dy).to_ne_bytes());
            vertices.extend_from_slice(&(-1.0f32).to_ne_bytes());
        }
        h.geometry
            .update_vertices(id, vertex_at..vertex_at + 36, &vertices)
            .expect("追加顶点");
        let index_at = snapshot.index_len();
        let base = (snapshot.vertex_len() / 12) as u16;
        let indices = [base, base + 1, base + 2];
        let mut bytes = Vec::with_capacity(6);
        for index in indices {
            bytes.extend_from_slice(&index.to_ne_bytes());
        }
        h.geometry
            .update_indices(id, index_at..index_at + 6, &bytes)
            .expect("追加索引");
    }

    /// X5：几何节点每块一条 lane，块间绘制序 = 块序，且每条块 lane 的 clip 语义完整
    /// （内容 clip 前缀/后缀与单 lane 用同一个 `emit_geometry_chunk` 实现）。
    #[test]
    fn geometry_node_gets_one_clipped_lane_per_chunk_in_order() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 10))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::Bounds);
        h.paint();

        assert_eq!(h.tree.lane_count(node), 2, "两块 = 两条块 lane");
        let first = h.tree.lane_at(node, 0).expect("块 0 lane");
        let second = h.tree.lane_at(node, 1).expect("块 1 lane");
        assert_eq!(h.scene.lane_geometry_chunk(first), Some((id, 0)));
        assert_eq!(h.scene.lane_geometry_chunk(second), Some((id, 1)));
        assert_eq!(
            h.order,
            vec![first, second],
            "块间绘制序 = 块序，两条 lane 相邻"
        );

        for lane in [first, second] {
            let primitives = h.scene.lane_primitives(lane).expect("lane 活着");
            assert!(
                matches!(primitives.first(), Some(Primitive::Clip(ClipOp::Push(_)))),
                "块 lane 必须以内容 clip 前缀开始"
            );
            assert!(
                matches!(primitives.last(), Some(Primitive::Clip(ClipOp::Pop))),
                "块 lane 必须以内容 clip 后缀结束"
            );
            assert_eq!(
                primitives
                    .iter()
                    .filter(|primitive| matches!(primitive, Primitive::Geometry(_)))
                    .count(),
                1,
                "每条块 lane 恰好一条几何绘制"
            );
        }
    }

    /// X5：几何数据只改尾块 ⇒ 增量帧只重建尾块 lane，前面的块一条都不动、绘制序不变。
    #[test]
    fn geometry_data_update_rebuilds_only_the_tail_chunk_lane() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 2))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        let first = h.tree.lane_at(node, 0).expect("块 0 lane");
        let second = h.tree.lane_at(node, 1).expect("块 1 lane");
        let first_version = h.scene.lane_state(first).expect("lane").0;
        let second_version = h.scene.lane_state(second).expect("lane").0;

        append_triangle(&mut h, id, 100.0, 100.0);
        assert_eq!(h.tree.mark_geometry_updated(id), 1, "绑定节点被标脏");
        let report = h.paint_incremental(&[node]);

        assert_eq!(report.rebuilt, 1, "只重建尾块 lane");
        assert_eq!(
            h.scene.lane_state(first).expect("lane").0,
            first_version,
            "第一块 lane 内容版本不动（编码器整段复用）"
        );
        assert_ne!(
            h.scene.lane_state(second).expect("lane").0,
            second_version,
            "尾块 lane 必须重建"
        );
        assert_eq!(h.order, vec![first, second], "增量帧不重排绘制序");
        assert!(
            report.damage.rects > 0 && !report.damage.full,
            "尾块新范围产生局部 damage（不是整帧）"
        );
    }

    /// RX-3（review-x 覆盖探针）：块 lane 的 damage 必须同时覆盖**旧块位置**与**新块位置**。
    ///
    /// 探针：删除 `paint_geometry_node` 里「旧块 bounds 并入 damage」后全量测试仍绿
    /// （只测了 store 层 `GeometryUpdate.damaged`）。本测试把整个三角形从 (10,10) 移到
    /// (100,60)：旧位置不在新块 AABB 内，而数据更新又**不并节点盒 damage** ⇒ 只有旧∪新
    /// 并集才能覆盖旧位置；只算新位置会静默留下残影。
    #[test]
    fn geometry_data_update_damage_covers_old_and_new_block_bounds() {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};

        fn triangle(x: f32, y: f32) -> Vec<u8> {
            let mut bytes = Vec::with_capacity(36);
            for (dx, dy) in [(0.0f32, 0.0f32), (1.0, 0.0), (0.0, 1.0)] {
                bytes.extend_from_slice(&(x + dx).to_ne_bytes());
                bytes.extend_from_slice(&(y + dy).to_ne_bytes());
                bytes.extend_from_slice(&(-1.0f32).to_ne_bytes());
            }
            bytes
        }

        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(GeometryDesc {
                vertex_bytes: bytes::Bytes::from(triangle(10.0, 10.0)),
                index_bytes: bytes::Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册单三角形几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        assert_eq!(h.tree.lane_count(node), 1, "单块几何只有一条块 lane");

        // 整体移动（旧 AABB 约 (10,10)-(11,11)，新 AABB 约 (100,60)-(101,61)）。
        h.geometry
            .update_vertices(id, 0..36, &triangle(100.0, 60.0))
            .expect("整体移动");
        assert_eq!(h.tree.mark_geometry_updated(id), 1);
        let report = h.paint_incremental(&[node]);
        assert!(!report.damage.full, "块移动是局部 damage，不得退化为整帧");
        assert_eq!(report.rebuilt, 1, "唯一块 lane 重建");

        let rects = h.damage.rects();
        let hits = |x: i32, y: i32| {
            rects
                .iter()
                .any(|rect| rect.x0 <= x && x < rect.x1 && rect.y0 <= y && y < rect.y1)
        };
        assert!(hits(10, 10), "旧位置必须重画（否则留残影）：{rects:?}");
        assert!(hits(100, 60), "新位置必须重画：{rects:?}");
    }

    /// X5：块满后追加 ⇒ 新建块 lane，增量路径用轻量 DFS 兜底重建绘制序；旧块 lane 不动。
    #[test]
    fn appending_across_the_chunk_boundary_creates_a_new_lane_and_rebuilds_order() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096))
            .expect("注册单块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        assert_eq!(h.tree.lane_count(node), 1);
        let first = h.tree.lane_at(node, 0).expect("块 0 lane");
        let first_version = h.scene.lane_state(first).expect("lane").0;

        append_triangle(&mut h, id, 100.0, 100.0);
        h.tree.mark_geometry_updated(id);
        let report = h.paint_incremental(&[node]);

        assert_eq!(report.rebuilt, 1);
        assert_eq!(h.tree.lane_count(node), 2, "满块后追加必须新建块 lane");
        let second = h.tree.lane_at(node, 1).expect("新块 lane");
        assert_eq!(h.scene.lane_geometry_chunk(second), Some((id, 1)));
        assert_eq!(h.order, vec![first, second], "绘制序兜底重建后包含新 lane");
        assert_eq!(
            h.scene.lane_state(first).expect("lane").0,
            first_version,
            "旧块 lane 不动"
        );
    }

    /// X5：没有数据变化的帧不重建任何块 lane（纯 damage / 滚动帧的 `encoded_lanes == 0` 前提）。
    #[test]
    fn unchanged_geometry_reuses_every_block_lane() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 10))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        let versions: Vec<u32> = (0..2)
            .map(|chunk| {
                let lane = h.tree.lane_at(node, chunk).expect("lane");
                h.scene.lane_state(lane).expect("lane").0
            })
            .collect();

        let report = h.paint_incremental(&[]);
        assert_eq!(report.rebuilt, 0, "没有变化 ⇒ 零重建");
        for (chunk, version) in versions.iter().enumerate() {
            let lane = h.tree.lane_at(node, chunk).expect("lane");
            assert_eq!(h.scene.lane_state(lane).expect("lane").0, *version);
        }
    }

    /// X5：per-draw 变换变化 ⇒ 全部块 lane 重建（变换烘在每条块 lane 的图元里）。
    #[test]
    fn geometry_transform_change_rebuilds_every_block_lane() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 10))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        let transform = crate::math::Transform::translate(7.0, 9.0);
        h.tree
            .set_geometry_transform(node, transform, Rect::default())
            .expect("几何节点");
        let report = h.paint_incremental(&[node]);
        assert_eq!(report.rebuilt, 2, "变换烘在块 lane 里 ⇒ 两块都要重建");
        for chunk in 0..2 {
            let lane = h.tree.lane_at(node, chunk).expect("lane");
            let primitives = h.scene.lane_primitives(lane).expect("lane");
            let Some(Primitive::Geometry(draw)) = primitives
                .iter()
                .find(|primitive| matches!(primitive, Primitive::Geometry(_)))
            else {
                panic!("块 {chunk} 必须有几何绘制");
            };
            assert_eq!(draw.transform, transform, "变换必须进每条块 lane");
        }
    }
}

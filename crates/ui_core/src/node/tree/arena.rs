//! 节点 arena：槽位分配 / 回收、句柄解析、旁表槽位生命周期。

use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{LayoutBox, Node, NodeKind, NodeSpec, NodeTree, layout::LayoutStats};
use crate::{
    error::EngineError,
    event::Action,
    interaction::NodeInteraction,
    layout::{LayoutOverride, MeasureFn},
    math::{Edges, Point, Rect, Size},
    node::{
        dependency::ReadCtx,
        dirty::{DirtyFlags, DirtySet},
        id::{Arena, NodeId},
        signal::SignalId,
    },
    paint::style::{PaintSpec, Style, StylePatch},
};

// 消费者：`impl NodeTree::new` 的薄委托（外部调用点见该方法文档）。
/// 建一棵只有根节点的树（消费者：`Window::new`）。
///
/// **根默认撑满 ICB**（`size: 100% × 100%`）：taffy 的 `compute_root_layout` 只接受
/// `available_space`，不会把「窗口尺寸」强加给根；flex/grid 根的 `size: auto` 是 shrink-to-fit
/// （实测：窗口 200×200 时根会塌成 0×0，百分比子节点全部为 0）。因此根必须在建树时显式撑满。
pub(crate) fn new() -> NodeTree {
    let mut root_style = Style::default();
    root_style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::percent(1.0),
        height: taffy::style::Dimension::percent(1.0),
    };
    let mut arena: Arena<Node> = Arena::new();
    let root = arena.insert_with(|id| Node {
        id,
        parent: None,
        children: Vec::new(),
        kind: NodeKind::Box,
        style: Box::new(root_style),
        style_patch: None,
        bind_style: None,
        measure: None,
        hit_shape: None,
        layout_override: None,
        tab_stop: true,
        paint: None,
        content_offset: Point::ZERO,
        text_bind: None,
        text_measured_size: Size::ZERO,
        text_content_width: 0.0,
        layout: LayoutBox::default(),
        local: Rect::default(),
        local_border: Edges::ZERO,
        laid_out_epoch: 0,
        damage: Rect::default(),
        lane: None,
        deps: None,
        dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        interaction: None,
        lane_offset: Point::ZERO,
    });
    let mut dirty_layout = DirtySet::new();
    dirty_layout.insert(root);
    let mut dirty_paint = DirtySet::new();
    dirty_paint.insert(root);
    let mut dirty_a11y = DirtySet::new();
    dirty_a11y.insert(root);
    let mut dirty_settle = DirtySet::new();
    dirty_settle.insert(root);
    NodeTree {
        arena,
        root,
        window: None,
        device_scale: None,
        hit_test_skips: std::cell::Cell::new(0),
        raw_origins: Vec::new(),
        layout_cache: ahash::HashMap::default(),
        text_slots: vec![None],
        debug_ids: vec![None],
        clip_keys: vec![None],
        subtree_bounds: vec![Rect::default()],
        a11y_slots: vec![None],
        dirty_bind: DirtySet::new(),
        dirty_layout,
        dirty_paint,
        dirty_a11y,
        dirty_settle,
        moved_nodes: DirtySet::new(),
        scroll_containers: Vec::new(),
        virtual_containers: Vec::new(),
        overlay_containers: Vec::new(),
        text_dirty: crate::node::tree::content::TextDirtyQueue::new(),
        text_layout_dirty: crate::node::tree::content::TextDirtyQueue::new(),
        removed_lanes: Vec::new(),
        hoisted: ahash::HashMap::default(),
        layout_epoch: 0,
        layout_stats: LayoutStats::default(),
        invalid_measure: None,
        geometry_damage_escalation: false,
        geometry_transforms: ahash::HashMap::default(),
        paint_marks: vec![0],
        paint_epoch: 0,
        scroll_slot_of: vec![0],
        structure_epoch: 0,
        painted_structure_epoch: 0,
        scroll_reg_epoch: 0,
        painted_scroll_reg_epoch: 0,
        force_full_paint: false,
        paint_visited: 0,
        paint_order_rebuilds: 0,
        paint_full_frames: 0,
        paint_incremental_frames: 0,
        dirty_scratch: Vec::new(),
        #[cfg(feature = "test-support")]
        scanned: std::cell::RefCell::new(ahash::HashSet::default()),
        bound_nodes: 0,
    }
}

// 消费者：`impl NodeTree::window_tag` 的薄委托（外部调用点见该方法文档）。
/// 树的窗口 tag（`0` = 未绑定）。
///
/// 消费者：`layout::taffy_tree::to_engine` —— taffy 只携带 64 位 tree-local 编码，
/// 回调里必须补上窗口 tag，否则返回的句柄是"通配"的，与 arena 里的键在 `HashMap`
/// （`layout_cache` / `hoisted` / `geometry_transforms`）里**不相等** ⇒ 缓存失效会漏清。
pub(crate) fn window_tag(tree: &NodeTree) -> u32 {
    tree.arena.scope()
}

// 消费者：`impl NodeTree::root` 的薄委托（外部调用点见该方法文档）。
/// 根句柄（永不失效）。
pub(crate) fn root(tree: &NodeTree) -> NodeId {
    tree.root
}

// 消费者：`impl NodeTree::bind_window` 的薄委托（外部调用点见该方法文档）。
/// 把树绑定到窗口：arena 的作用域 + 根句柄重贴 + 按根登记的脏集合重建。
///
/// 必须在建树之后、挂任何子节点之前调用（`Window::new` 之后立刻调用，见 `App::create_window`）。
/// 绑定后新铸的每个 `NodeId` 都带窗口 tag，跨窗口误用在 `NodeTree::get` 处确定性失败。
pub(crate) fn bind_window(tree: &mut NodeTree, window: crate::window::WindowId) {
    debug_assert!(
        tree.window.is_none(),
        "窗口只能绑定一次（NodeTree::bind_window）"
    );
    debug_assert_eq!(
        tree.arena.len(),
        1,
        "窗口必须在挂载任何子节点之前绑定（只有根节点存在）"
    );
    tree.arena.set_scope(window.to_bits());
    let root = tree.root.with_window_tag(window.to_bits());
    if let Some(node) = tree.arena.get_mut(tree.root) {
        node.retag(root);
    }
    tree.root = root;
    tree.window = Some(window);
    // 构造期只有根进入了这四个集合（用旧键）；绑定后必须换成带窗口 tag 的键。
    for set in [
        &mut tree.dirty_layout,
        &mut tree.dirty_paint,
        &mut tree.dirty_a11y,
        &mut tree.dirty_settle,
    ] {
        set.clear();
        set.insert(root);
    }
}

// 消费者：`impl NodeTree::get` 的薄委托（外部调用点见该方法文档）。
/// 取节点；陈旧句柄 → `EngineError::StaleNode`。
///
/// # Errors
/// `EngineError::StaleNode { node }`：节点从未存在或已卸载。
pub(crate) fn get(tree: &NodeTree, node: NodeId) -> Result<&Node, EngineError> {
    tree.arena.get(node).ok_or(EngineError::StaleNode { node })
}

// 消费者：`impl NodeTree::len` 的薄委托（外部调用点见该方法文档）。
/// 存活节点数（O(1)：`Arena::len` = 槽位数 − 自由表长度）。
pub(crate) fn len(tree: &NodeTree) -> usize {
    tree.arena.len()
}

// 消费者：`impl NodeTree::insert_child` 的薄委托（外部调用点见该方法文档）。
/// 铸一个新节点并挂到 `parent` 末尾（消费者：`NodeBuilder::build`）。
///
/// 参数是 [`NodeSpec`]（部件）而不是半成品 `Node`：`Node` 的全部字段都是本模块私有的，
/// 只有这里能构造（P03 F-43 / P04 S-40）。
///
/// **成本 O(深度)**：不再维护全序数组，挂载只做「arena 分配 + 父的 `children` 追加 +
/// 标脏冒泡」，与已有兄弟数量无关（旧实现的 `order_insert` 是 O(N²) 建树，R03-2）。
pub(crate) fn insert_child(
    tree: &mut NodeTree,
    parent: NodeId,
    spec: NodeSpec,
) -> Result<NodeId, EngineError> {
    tree.get(parent)?;
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
        a11y,
        bind_a11y,
        tab_stop,
    } = spec;
    // 初值：挂了动态闭包的节点必须进 `dirty_bind`（否则首帧 `refresh_styles` 不会求值它，
    // 旧实现靠"新节点带 PAINT"间接达到同一效果 —— 那是 BIND/PAINT 混用的遗产，R03-9）。
    let bound = bind_style.is_some() || bind_text.is_some() || bind_a11y.is_some();
    let interaction = (action.is_some() || drag.is_some() || focus_ring.is_some()).then(|| {
        let mut slot = NodeInteraction::new();
        slot.action = action;
        slot.drag = drag;
        slot.focus_ring = focus_ring;
        Box::new(slot)
    });
    let id = tree.arena.insert_with(move |id| Node {
        id,
        parent: None,
        children: Vec::new(),
        kind,
        style: Box::new(style),
        style_patch: None,
        bind_style,
        measure,
        hit_shape: hit_shape.map(Box::new),
        layout_override,
        tab_stop,
        paint,
        content_offset: Point::ZERO,
        text_bind: bind_text,
        text_measured_size: Size::ZERO,
        text_content_width: 0.0,
        layout: LayoutBox::default(),
        local: Rect::default(),
        local_border: Edges::ZERO,
        laid_out_epoch: 0,
        damage: Rect::default(),
        lane: None,
        deps: None,
        dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        interaction,
        lane_offset: Point::ZERO,
    });
    // 槽位复用：上一代节点的 taffy 缓存必须丢弃，否则新节点继承陈旧布局（P04 §6.1）。
    tree.layout_cache.remove(&id);
    let index = id.index() as usize;
    // N5：文本形状快照与 taffy 缓存同生命周期 —— 复用槽位时必须丢弃上一代的 `Rc<ShapedText>`。
    if tree.text_slots.len() <= index {
        tree.text_slots.resize_with(index + 1, || None);
    }
    tree.text_slots[index] = None;
    // P1-2.3：新挂载的文本节点必须进脏集合，否则 `refresh_text` 永远不会 shape 它
    // （静态 `text()` 与 `bind_text` 都走这条路 —— 前者挂载时写内容，后者首次求值写内容）。
    if tree
        .arena
        .get(id)
        .is_some_and(|target| target.kind.text().is_some())
    {
        tree.mark_text_dirty(id);
    }
    if bound {
        tree.bound_nodes += 1;
        let _ = tree.touch(id, DirtyFlags::BIND);
    }
    // 稳定调试 id 与 clip 上下文同生命周期：槽位复用不得继承上一代的值（P06 §5.6 / C4）。
    if tree.debug_ids.len() <= index {
        tree.debug_ids.resize_with(index + 1, || None);
    }
    tree.debug_ids[index] = debug_id;
    if tree.clip_keys.len() <= index {
        tree.clip_keys.resize_with(index + 1, || None);
    }
    tree.clip_keys[index] = None;
    // A1：槽位复用不得继承上一代节点的 paint 剪枝侧表（闭包戳记 / 滚动槽缓存）。
    if tree.paint_marks.len() <= index {
        tree.paint_marks.resize(index + 1, 0);
    }
    tree.paint_marks[index] = 0;
    if tree.scroll_slot_of.len() <= index {
        tree.scroll_slot_of.resize(index + 1, 0);
    }
    tree.scroll_slot_of[index] = 0;
    if tree.subtree_bounds.len() <= index {
        tree.subtree_bounds.resize_with(index + 1, Rect::default);
    }
    // 新节点还没有布局盒：子树包围盒先清空，absolutize 会重算。
    tree.subtree_bounds[index] = Rect::default();
    // a11y 载荷同生命周期：槽位复用不得继承上一代节点的声明/补丁（P11 F-114）。
    if tree.a11y_slots.len() <= index {
        tree.a11y_slots.resize_with(index + 1, || None);
    }
    tree.a11y_slots[index] = (a11y.is_some() || bind_a11y.is_some())
        .then(|| Box::new(crate::a11y::tree::NodeA11y::new(a11y, bind_a11y)));
    // 新节点的初值脏位是 `LAYOUT | PAINT | A11Y`：反向集合必须同步（否则门禁看不到它）。
    tree.dirty_layout.insert(id);
    tree.dirty_paint.insert(id);
    tree.dirty_a11y.insert(id);
    tree.dirty_settle.insert(id);
    tree.hoisted.remove(&id);
    if let Some(node) = tree.arena.get_mut(id) {
        node.parent = Some(parent);
    }
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.push(id);
    }
    tree.mark_structure(parent);
    Ok(id)
}

// 消费者：`impl NodeTree::append_child` 的薄委托（外部调用点见该方法文档）。
/// 把 `child`（连同子树）挂到 `parent` 的末尾；`child` 已有父则先摘下。
///
/// 没有全序数组要拼接（R03-2）：成本 O(子树 + 深度)，与兄弟数量无关。
///
/// # Errors
/// `EngineError::StaleNode`：`parent`/`child` 任一陈旧。
///
/// # Panics (debug)
/// 形成结构环（`child` 是 `parent` 的祖先或二者相同）：debug 断言失败，release 下**不改结构**。
pub(crate) fn append_child(
    tree: &mut NodeTree,
    parent: NodeId,
    child: NodeId,
) -> Result<(), EngineError> {
    tree.get(parent)?;
    tree.get(child)?;
    if parent == child || tree.is_ancestor(child, parent) {
        debug_assert!(
            false,
            "structural cycle: cannot append an ancestor as a child"
        );
        return Ok(());
    }

    if let Some(old_parent) = tree.get(child)?.parent {
        if let Some(p) = tree.arena.get_mut(old_parent) {
            p.children.retain(|existing| *existing != child);
        }
        tree.mark_structure(old_parent);
        // 整棵被移动的子树都要重画：它的绘制顺序在搬动后可能改变。
        tree.mark_subtree_paint(child);
    }

    if let Some(node) = tree.arena.get_mut(child) {
        node.parent = Some(parent);
    }
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.push(child);
    }
    tree.mark_structure(parent);
    Ok(())
}

// 消费者：`impl NodeTree::remove` 的薄委托（外部调用点见该方法文档）。
/// 卸载 `node` 及其整棵子树（句柄全部变为陈旧）。
///
/// 删除子树，并把被删节点的 `(NodeId, deps)` 交还调用方（P1-2.5）。
///
/// 依赖图按 `SignalId → OwnerSet` 索引，节点卸载**不会**自动解开边；`App` 必须拿着这些
/// `deps` 逐条 `graph.detach`。旧实现只返回 `Ok(())`，于是虚拟列表反复滚动、浮层反复开关
/// 都会留下死边，`App::write`（最高频热路径）的成本从 O(活跃读者) 退化成 O(历史挂载过的
/// 所有读者)，并且每次写都要做一次堆分配。
///
/// 对齐 GPUI 的 `SubscriberSet`（entity drop 时同步移除订阅）与 Qt 的 `QObject` 析构
/// （自动断开 signal-slot）。
///
/// # Errors
/// `EngineError::StaleNode`：`node` 陈旧，或 `node` 是 root（root 不是可移除的子节点，见 N11）。
pub(crate) fn remove(
    tree: &mut NodeTree,
    node: NodeId,
) -> Result<Vec<(NodeId, SmallVec<[SignalId; 8]>)>, EngineError> {
    if node == tree.root {
        return Err(EngineError::StaleNode { node });
    }
    let parent = tree
        .get(node)?
        .parent
        .expect("non-root node always has a parent");
    if let Some(p) = tree.arena.get_mut(parent) {
        p.children.retain(|existing| *existing != node);
    }
    // 子树句柄快照（DFS 先序）。之后 arena 里就查不到了。
    let mut block: Vec<NodeId> = Vec::with_capacity(16);
    tree.for_each_subtree(node, |id| block.push(id));
    let removed: ahash::HashSet<NodeId> = block.iter().copied().collect();
    // 几何节点的可见范围可能超出布局盒：卸载/移动/隐藏三者都拿不到 desc，
    // 保守升级为全帧 damage（否则溢出部分在「不清屏」前提下长期残影；复审 A3）。
    if block.iter().any(|id| {
        tree.arena
            .get(*id)
            .is_some_and(|target| matches!(target.kind, NodeKind::Geometry(_)))
    }) {
        tree.geometry_damage_escalation = true;
    }
    // 旧像素必须重绘：被卸载子树的包围盒并入父的 damage（出流/覆盖节点可能超出父盒，
    // 不能只靠 `mark_structure` 的父盒；P06 §5.5）。
    let mut subtree_damage = Rect::default();
    for id in &block {
        if let Some(target) = tree.arena.get(*id) {
            subtree_damage = subtree_damage
                .union(target.damage)
                .union(target.layout.bounds);
            if let Some(lane) = target.lane {
                tree.removed_lanes.push(lane);
            }
        }
    }
    if let Some(p) = tree.arena.get_mut(parent) {
        p.damage = p.damage.union(subtree_damage);
    }
    // 卸载前收集每个被删节点仍持有的依赖边（节点本身马上就查不到了）。
    let mut detached: Vec<(NodeId, SmallVec<[SignalId; 8]>)> = Vec::with_capacity(block.len());
    for id in block.iter().rev() {
        let index = id.index() as usize;
        tree.layout_cache.remove(id);
        if let Some(slot) = tree.text_slots.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.debug_ids.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.clip_keys.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.paint_marks.get_mut(index) {
            *slot = 0;
        }
        if let Some(slot) = tree.scroll_slot_of.get_mut(index) {
            *slot = 0;
        }
        if let Some(slot) = tree.a11y_slots.get_mut(index) {
            *slot = None;
        }
        // 反向脏集合必须同步：陈旧节点若留在集合里，O(1) 门禁会永远为真（空转帧）。
        tree.dirty_bind.remove(*id);
        tree.dirty_layout.remove(*id);
        tree.dirty_paint.remove(*id);
        tree.dirty_a11y.remove(*id);
        tree.dirty_settle.remove(*id);
        tree.moved_nodes.remove(*id);
        tree.hoisted.remove(id);
        tree.geometry_transforms.remove(id);
        let had_bind = tree
            .arena
            .get(*id)
            .is_some_and(|target| target.bind_style.is_some() || target.text_bind.is_some())
            || tree
                .a11y_slots
                .get(id.index() as usize)
                .and_then(|slot| slot.as_deref())
                .is_some_and(|slot| slot.bind.is_some());
        if had_bind {
            tree.bound_nodes = tree.bound_nodes.saturating_sub(1);
        }
        if let Some(target) = tree.arena.remove(*id)
            && let Some(deps) = target.deps
            && !deps.is_empty()
        {
            detached.push((*id, *deps));
        }
    }
    // 别的包含块可能把被卸载的节点记在自己的 hoisted 列表里（出流孙节点）⇒ 逐表剔除。
    if !tree.hoisted.is_empty() {
        tree.hoisted.retain(|_, list| {
            list.retain(|child| !removed.contains(child));
            !list.is_empty()
        });
    }
    // 容器登记表同样剔除（否则每帧的 clamp/rewindow 会一直空转在陈旧句柄上）。
    if !tree.scroll_containers.is_empty() {
        tree.scroll_containers.retain(|id| !removed.contains(id));
    }
    if !tree.virtual_containers.is_empty() {
        tree.virtual_containers.retain(|id| !removed.contains(id));
    }
    if !tree.overlay_containers.is_empty() {
        tree.overlay_containers.retain(|id| !removed.contains(id));
    }
    // 集合里可能留了大量陈旧条目（子树整块卸载）：顺手压缩，保持迭代成本与内容一致。
    tree.dirty_bind.shrink_stale();
    tree.dirty_layout.shrink_stale();
    tree.dirty_paint.shrink_stale();
    tree.dirty_a11y.shrink_stale();
    tree.dirty_settle.shrink_stale();
    tree.moved_nodes.shrink_stale();
    tree.mark_structure(parent);
    // B9：卸载后局部收紧祖先的子树包络（被删子树不再参与后续 `place_subtree` 的访问）。
    super::layout::recompute_bounds_upwards(tree, parent);
    Ok(detached)
}

// 消费者：`impl NodeTree::debug_id` 的薄委托（外部调用点见该方法文档）。
/// 稳定调试 id（`NodeBuilder::id` 的只读面；消费者：`Probe`、诊断日志）。
///
/// 同一棵树内**允许重复**：`Selector::Id` 取 DFS 先序的第一个（确定性），不做唯一性检查。
pub(crate) fn debug_id(tree: &NodeTree, node: NodeId) -> Option<&str> {
    tree.debug_ids
        .get(node.index() as usize)
        .and_then(|slot| slot.as_ref())
        .map(SmolStr::as_str)
}

// 消费者：`NodeBuilder::build`（`node/builder.rs`）与 `NodeTree::insert_child`。
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
            a11y: None,
            bind_a11y: None,
            tab_stop: true,
        }
    }

    pub(crate) fn with_a11y(mut self, over: crate::a11y::A11yOverride) -> Self {
        self.a11y = Some(over);
        self
    }

    pub(crate) fn with_bind_a11y(
        mut self,
        bind: Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>,
    ) -> Self {
        self.bind_a11y = Some(bind);
        self
    }

    pub(crate) fn with_tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
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

    pub(crate) fn with_hit_shape(mut self, hit_shape: crate::layout::hit_test::HitShape) -> Self {
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

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
        dirty::DirtyFlags,
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
        children: SmallVec::new(),
        kind: NodeKind::Box,
        style: root_style,
        style_patch: StylePatch::default(),
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
    NodeTree {
        arena,
        root,
        order: vec![root],
        layout_cache: vec![None],
        text_slots: vec![None],
        debug_ids: vec![None],
        clip_keys: vec![None],
        subtree_bounds: vec![Rect::default()],
        a11y_slots: vec![None],
        a11y_dirty: 1,
        text_dirty: Vec::new(),
        removed_lanes: Vec::new(),
        hoisted: ahash::HashMap::default(),
        layout_epoch: 0,
        layout_stats: LayoutStats::default(),
        invalid_measure: None,
        geometry_damage_escalation: false,
    }
}

// 消费者：`impl NodeTree::root` 的薄委托（外部调用点见该方法文档）。
/// 根句柄（永不失效）。
pub(crate) fn root(tree: &NodeTree) -> NodeId {
    tree.root
}

// 消费者：`impl NodeTree::get` 的薄委托（外部调用点见该方法文档）。
/// 取节点；陈旧句柄 → `EngineError::StaleNode`。
///
/// # Errors
/// `EngineError::StaleNode { node }`：节点从未存在或已卸载。
pub(crate) fn get(tree: &NodeTree, node: NodeId) -> Result<&Node, EngineError> {
    tree.arena.get(node).ok_or(EngineError::StaleNode { node })
}

// 消费者：`impl NodeTree::insert_child` 的薄委托（外部调用点见该方法文档）。
/// 铸一个新节点并挂到 `parent` 末尾（消费者：`NodeBuilder::build`）。
///
/// 参数是 [`NodeSpec`]（部件）而不是半成品 `Node`：`Node` 的全部字段都是本模块私有的，
/// 只有这里能构造（P03 F-43 / P04 S-40）。
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
        children: SmallVec::new(),
        kind,
        style,
        style_patch: StylePatch::default(),
        bind_style,
        measure,
        hit_shape,
        layout_override,
        tab_stop,
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
    if tree.layout_cache.len() <= index {
        tree.layout_cache.resize_with(index + 1, || None);
    }
    tree.layout_cache[index] = None;
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
    // 稳定调试 id 与 clip 上下文同生命周期：槽位复用不得继承上一代的值（P06 §5.6 / C4）。
    if tree.debug_ids.len() <= index {
        tree.debug_ids.resize_with(index + 1, || None);
    }
    tree.debug_ids[index] = debug_id;
    if tree.clip_keys.len() <= index {
        tree.clip_keys.resize_with(index + 1, || None);
    }
    tree.clip_keys[index] = None;
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
    // 新节点恒带 `A11Y`（`dirty` 的初值），计数必须同步。
    tree.a11y_dirty = tree.a11y_dirty.saturating_add(1);
    tree.hoisted.remove(&id);
    tree.order_insert(parent, id);
    if let Some(node) = tree.arena.get_mut(id) {
        node.parent = Some(parent);
    }
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.push(id);
    }
    tree.mark_structure(parent);
    #[cfg(debug_assertions)]
    tree.debug_assert_order_matches_tree();
    Ok(id)
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
    tree.order_remove_subtree(node);
    if let Some(p) = tree.arena.get_mut(parent) {
        p.children.retain(|existing| *existing != node);
    }
    let block = tree.collect_subtree_ids(node);
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
    // 不能只靠 `mark_structure` 的父盒；P06 §5.5、不加 `LayoutBox.overflow` 的依据）。
    let mut subtree_damage = Rect::default();
    for id in &block {
        if let Some(target) = tree.arena.get(*id) {
            subtree_damage = subtree_damage
                .union(target.damage)
                .union(target.layout.bounds);
            tree.removed_lanes.extend(target.lanes.iter().copied());
        }
    }
    if let Some(p) = tree.arena.get_mut(parent) {
        p.damage = p.damage.union(subtree_damage);
    }
    // 卸载前收集每个被删节点仍持有的依赖边（节点本身马上就查不到了）。
    let mut detached: Vec<(NodeId, SmallVec<[SignalId; 8]>)> = Vec::with_capacity(block.len());
    for id in block.iter().rev() {
        let index = id.index() as usize;
        if let Some(slot) = tree.layout_cache.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.text_slots.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.debug_ids.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.clip_keys.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.a11y_slots.get_mut(index) {
            *slot = None;
        }
        if tree
            .arena
            .get(*id)
            .is_some_and(|target| target.dirty().contains(DirtyFlags::A11Y))
        {
            tree.a11y_dirty = tree.a11y_dirty.saturating_sub(1);
        }
        tree.hoisted.remove(id);
        if let Some(target) = tree.arena.remove(*id)
            && !target.deps.is_empty()
        {
            detached.push((*id, target.deps));
        }
    }
    // 别的包含块可能把被卸载的节点记在自己的 hoisted 列表里（出流孙节点）⇒ 逐表剔除。
    tree.hoisted.retain(|_, list| {
        list.retain(|child| !block.contains(child));
        !list.is_empty()
    });
    tree.mark_structure(parent);
    #[cfg(debug_assertions)]
    tree.debug_assert_order_matches_tree();
    Ok(detached)
}

// 消费者：`impl NodeTree::debug_id` 的薄委托（外部调用点见该方法文档）。
/// 稳定调试 id（`NodeBuilder::id` 的只读面；消费者：`Probe`、诊断日志）。
///
/// 同一棵树内**允许重复**：`Selector::Id` 取 `order()` 的第一个（确定性），不做唯一性检查。
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

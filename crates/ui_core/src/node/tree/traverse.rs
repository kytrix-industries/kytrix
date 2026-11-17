//! 遍历、可见性与容器登记：DFS 先序（唯一的"全树顺序"来源）、父链判定、hoisted 包含块表、
//! 滚动 / 虚拟列表容器的反向索引。
//!
//! 本模块**没有**"全序数组"：绘制序、命中序、a11y 快照序、Tab 序全部由 [`for_each_dfs`]
//! （`children` 的 DFS 先序）现算。旧实现维护 `order: Vec<NodeId>`，结构变更因此要拼接全序
//! 数组（O(N²) 建树，R03-2）。

use smallvec::SmallVec;

use super::NodeTree;
use crate::node::id::NodeId;

/// DFS 先序遍历整棵树的迭代式实现（显式栈，深度与树深无关；不分配）。
///
/// 消费者：a11y 全树快照、`App::close_window` 依赖退订、诊断 / probe、测试。
pub(crate) fn for_each_dfs(tree: &NodeTree, mut visit: impl FnMut(NodeId)) {
    let mut stack: SmallVec<[NodeId; 32]> = SmallVec::new();
    stack.push(tree.root);
    while let Some(current) = stack.pop() {
        visit(current);
        #[cfg(feature = "test-support")]
        tree.note_scanned(current);
        if let Some(target) = tree.arena.get(current) {
            stack.extend(target.children.iter().rev().copied());
        }
    }
}

/// DFS 先序句柄快照（分配；只给需要"先收集再改树"的冷路径用：Tab 序、a11y 全树）。
pub(crate) fn dfs_ids(tree: &NodeTree) -> Vec<NodeId> {
    let mut ids = Vec::with_capacity(tree.arena.len());
    for_each_dfs(tree, |id| ids.push(id));
    ids
}

/// 子树 DFS 先序（含 `root` 自己；迭代式，不分配）。
///
/// 消费者：`mark_subtree_paint`、`remove`（卸载子树时的句柄快照）。
pub(crate) fn for_each_subtree(tree: &NodeTree, root: NodeId, mut visit: impl FnMut(NodeId)) {
    let mut stack: SmallVec<[NodeId; 16]> = SmallVec::new();
    stack.push(root);
    while let Some(current) = stack.pop() {
        visit(current);
        #[cfg(feature = "test-support")]
        tree.note_scanned(current);
        if let Some(target) = tree.arena.get(current) {
            stack.extend(target.children.iter().rev().copied());
        }
    }
}

// 消费者：`impl NodeTree::is_visible_locally` 的薄委托（外部调用点见该方法文档）。
/// 该节点**自身**是否可见（`display != None` 且 `visible != Some(false)`）。
///
/// 这是「不可见」的**唯一**定义：命中测试、焦点遍历、a11y 用同一条规则。命中测试在递归里
/// 对本节点调用它（于是整棵子树被排除）；需要一次性判断某节点时用
/// [`NodeTree::is_effectively_visible`]（它还会检查祖先）。
pub(crate) fn is_visible_locally(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena.get(node).is_some_and(|target| {
        !matches!(target.style.inner.display, taffy::style::Display::None)
            && target.style_patch().visible != Some(false)
    })
}

// 消费者：`impl NodeTree::is_effectively_visible` 的薄委托（外部调用点见该方法文档）。
/// 该节点**及其所有祖先**都可见。
///
/// `visible: false` / `display: none` 都按 CSS 语义隐藏整棵子树，因此判断「能不能被聚焦」
/// 「能不能被 Tab 到」时不能只看本节点 —— 旧实现的焦点遍历只查了 `interactive`，于是 Tab
/// 会落到不可见控件上，Enter 还能触发隐藏的 action（P2-3.3）。
pub(crate) fn is_effectively_visible(tree: &NodeTree, node: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if !is_visible_locally(tree, candidate) {
            return false;
        }
        current = tree.arena.get(candidate).and_then(|target| target.parent);
    }
    true
}

// 消费者：`impl NodeTree::is_ancestor` 的薄委托（外部调用点见该方法文档）。
/// `ancestor` 是否是 `node` 的（严格）祖先。
pub(crate) fn is_ancestor(tree: &NodeTree, ancestor: NodeId, node: NodeId) -> bool {
    let mut cursor = tree.arena.get(node).and_then(|target| target.parent);
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
    false
}

// 消费者：`impl NodeTree::hoisted_clear` 的薄委托（外部调用点见该方法文档）。
/// `LayoutContainingBlock::clear_hoisted_children`。
pub(crate) fn hoisted_clear(tree: &mut NodeTree, node: NodeId) {
    if let Some(list) = tree.hoisted.get_mut(&node) {
        list.clear();
    }
}

// 消费者：`impl NodeTree::hoisted_push` 的薄委托（外部调用点见该方法文档）。
/// `LayoutContainingBlock::add_hoisted_children`。
pub(crate) fn hoisted_push(tree: &mut NodeTree, node: NodeId, hoisted: &[NodeId]) {
    tree.hoisted
        .entry(node)
        .or_default()
        .extend_from_slice(hoisted);
}

// 消费者：`impl NodeTree::note_scroll_container` 的薄委托（外部调用点见该方法文档）。
/// 登记一个滚动容器（挂载 `ScrollSlot` 时调用一次）。
///
/// 反向索引的意义：`App::clamp_scroll_offsets` 每帧要重夹所有滚动偏移，旧实现为此遍历全树
/// （R03-1 的 #1/#9）。登记后成本是 O(滚动容器数)，与节点总数无关。
pub(crate) fn note_scroll_container(tree: &mut NodeTree, node: NodeId) {
    if !tree.scroll_containers.contains(&node) {
        tree.scroll_containers.push(node);
    }
}

// 消费者：`impl NodeTree::note_virtual_container` 的薄委托（外部调用点见该方法文档）。
/// 登记一个虚拟列表容器（挂载 `VirtualSlot` 时调用一次）。
pub(crate) fn note_virtual_container(tree: &mut NodeTree, node: NodeId) {
    if !tree.virtual_containers.contains(&node) {
        tree.virtual_containers.push(node);
    }
}

// 消费者：`impl NodeTree::note_overlay_container` 的薄委托（外部调用点见该方法文档）。
/// 登记一个浮层节点（`App::create_overlay` 时调用一次）。
///
/// 浮层的锚点纠位每帧都要跑（`resolve_overlay_placements`），登记后成本是 O(浮层数)，
/// 而不是 O(root 的子节点数)。
pub(crate) fn note_overlay_container(tree: &mut NodeTree, node: NodeId) {
    if !tree.overlay_containers.contains(&node) {
        tree.overlay_containers.push(node);
    }
}

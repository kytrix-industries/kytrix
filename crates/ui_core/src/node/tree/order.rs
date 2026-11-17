//! `order`（DFS 先序数组）的维护：局部拼接、子树块长度、祖先判定。

use smallvec::SmallVec;

use super::NodeTree;
use crate::{error::EngineError, node::id::NodeId};

// 消费者：`impl NodeTree::order` 的薄委托（外部调用点见该方法文档）。
/// DFS 先序句柄数组（`root` 在首位）。消费者：`App::refresh_styles`、`App::close_window`。
pub(crate) fn order(tree: &NodeTree) -> &[NodeId] {
    &tree.order
}

// 消费者：`impl NodeTree::append_child` 的薄委托（外部调用点见该方法文档）。
/// 把 `child`（连同子树）挂到 `parent` 的末尾；`child` 已有父则先摘下（局部拼接，不重建整树）。
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
        tree.order_remove_subtree(child);
        if let Some(p) = tree.arena.get_mut(old_parent) {
            p.children.retain(|existing| *existing != child);
        }
        tree.mark_structure(old_parent);
        // 整棵被移动的子树都要重画：它的绘制桶顺序（`order`）在拼接后可能改变。
        tree.mark_subtree_paint(child);
    }

    tree.order_insert(parent, child);
    if let Some(node) = tree.arena.get_mut(child) {
        node.parent = Some(parent);
    }
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.push(child);
    }
    tree.mark_structure(parent);
    #[cfg(debug_assertions)]
    tree.debug_assert_order_matches_tree();
    Ok(())
}

// 消费者：`impl NodeTree::order_insert` 的薄委托（外部调用点见该方法文档）。
/// 把 `child` 的子树块插到 `parent` 的子节点区的**末尾**（局部拼接）。
pub(crate) fn order_insert(tree: &mut NodeTree, parent: NodeId, child: NodeId) {
    let position = {
        let start = tree
            .order_pos(parent)
            .map_or(tree.order.len(), |pos| pos + 1);
        let mut position = start;
        if let Some(target) = tree.arena.get(parent) {
            for existing in &target.children {
                if *existing != child {
                    position += tree.subtree_len(*existing);
                }
            }
        }
        position
    };
    let block = tree.collect_subtree_ids(child);
    for (offset, id) in block.iter().enumerate() {
        tree.order.insert(position + offset, *id);
    }
}

// 消费者：`impl NodeTree::order_remove_subtree` 的薄委托（外部调用点见该方法文档）。
/// 从 `order` 里删掉 `node` 的子树块（调用前该块必须是连续的）。
pub(crate) fn order_remove_subtree(tree: &mut NodeTree, node: NodeId) {
    let Some(position) = tree.order_pos(node) else {
        return;
    };
    let len = tree.subtree_len(node);
    tree.order.drain(position..position + len);
}

// 消费者：`impl NodeTree::order_pos` 的薄委托（外部调用点见该方法文档）。
/// `node` 在 `order` 里的位置（O(n) 线性扫描；P03 的最简版，见 V-28）。
pub(crate) fn order_pos(tree: &NodeTree, node: NodeId) -> Option<usize> {
    tree.order.iter().position(|id| *id == node)
}

// 消费者：`impl NodeTree::subtree_len` 的薄委托（外部调用点见该方法文档）。
/// 子树节点数（迭代 DFS，不递归，深树也不爆栈）。
pub(crate) fn subtree_len(tree: &NodeTree, node: NodeId) -> usize {
    let mut count = 0;
    let mut stack = SmallVec::<[NodeId; 16]>::new();
    stack.push(node);
    while let Some(current) = stack.pop() {
        count += 1;
        if let Some(target) = tree.arena.get(current) {
            stack.extend(target.children.iter().copied());
        }
    }
    count
}

// 消费者：`impl NodeTree::collect_subtree_ids` 的薄委托（外部调用点见该方法文档）。
/// 子树的 DFS 先序 id 序列。
pub(crate) fn collect_subtree_ids(tree: &NodeTree, node: NodeId) -> SmallVec<[NodeId; 16]> {
    let mut ids = SmallVec::new();
    let mut stack = SmallVec::<[NodeId; 16]>::new();
    stack.push(node);
    while let Some(current) = stack.pop() {
        ids.push(current);
        if let Some(target) = tree.arena.get(current) {
            for child in target.children.iter().rev() {
                stack.push(*child);
            }
        }
    }
    ids
}

// 消费者：`impl NodeTree::debug_assert_order_matches_tree` 的薄委托（外部调用点见该方法文档）。
/// debug-only：`order` 必须是**全序**（DFS 先序、无重复、无遗漏、root 在首位）。
#[cfg(debug_assertions)]
pub(crate) fn debug_assert_order_matches_tree(tree: &NodeTree) {
    let mut expected = SmallVec::<[NodeId; 32]>::new();
    let mut stack = SmallVec::<[NodeId; 32]>::new();
    stack.push(tree.root);
    while let Some(current) = stack.pop() {
        expected.push(current);
        if let Some(target) = tree.arena.get(current) {
            for child in target.children.iter().rev() {
                stack.push(*child);
            }
        }
    }
    debug_assert_eq!(
        expected.as_slice(),
        tree.order.as_slice(),
        "order 必须是 DFS 先序全序"
    );
}

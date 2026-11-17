//! 遍历与可见性：父链判定、hoisted 包含块表、可见性规则。

use super::NodeTree;
use crate::node::id::NodeId;

// 消费者：`impl NodeTree::is_visible_locally` 的薄委托（外部调用点见该方法文档）。
/// 该节点**自身**是否可见（`display != None` 且 `visible != Some(false)`）。
///
/// 这是「不可见」的**唯一**定义：命中测试、焦点遍历、a11y 用同一条规则。命中测试在递归里
/// 对本节点调用它（于是整棵子树被排除）；需要一次性判断某节点时用
/// [`NodeTree::is_effectively_visible`]（它还会检查祖先）。
pub(crate) fn is_visible_locally(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena.get(node).is_some_and(|target| {
        !matches!(target.style.inner.display, taffy::style::Display::None)
            && target.style_patch.visible != Some(false)
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
        if !tree.is_visible_locally(candidate) {
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

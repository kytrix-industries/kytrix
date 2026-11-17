//! 脏位：`mark_*` 系列、向上冒泡、`touch` / `any_node`、a11y 脏位计数。

use smallvec::SmallVec;

use super::{Node, NodeTree};
use crate::node::{dirty::DirtyFlags, id::NodeId};

// 消费者：`impl NodeTree::mark_paint` 的薄委托（外部调用点见该方法文档）。
/// 标 `PAINT`（不冒泡）。返回 `false` = 句柄陈旧。
pub(crate) fn mark_paint(tree: &mut NodeTree, node: NodeId) -> bool {
    tree.touch(node, DirtyFlags::PAINT)
}

// 消费者：`impl NodeTree::mark_layout` 的薄委托（外部调用点见该方法文档）。
/// 标 `LAYOUT | PAINT | A11Y` 并向上冒泡（§9.2 / N7），同时清 `node..root` 的 taffy 缓存（P04 §6.2）。
///
/// `A11Y` 是 P11 追加的：bounds 变了 ⇒ a11y bounds 也变了，必须重发（S-109）。它不产生
/// damage、不清 `REUSABLE`（`touch` 只在含 `LAYOUT|PAINT` 时做那两件事）。
pub(crate) fn mark_layout(tree: &mut NodeTree, node: NodeId) -> bool {
    if !tree.touch(
        node,
        DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
    ) {
        return false;
    }
    tree.bubble_layout(node);
    tree.invalidate_caches_upwards(node);
    true
}

// 消费者：`impl NodeTree::mark_a11y` 的薄委托（外部调用点见该方法文档）。
/// 标 `A11Y`（不冒泡）。
pub(crate) fn mark_a11y(tree: &mut NodeTree, node: NodeId) -> bool {
    tree.touch(node, DirtyFlags::A11Y)
}

// 消费者：`impl NodeTree::mark_structure` 的薄委托（外部调用点见该方法文档）。
/// 结构变更：标 `LAYOUT|PAINT|ORDER|A11Y` 并向上冒泡，同时清 `node..root` 的 taffy 缓存。
pub(crate) fn mark_structure(tree: &mut NodeTree, node: NodeId) -> bool {
    if !tree.touch(
        node,
        DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::ORDER | DirtyFlags::A11Y,
    ) {
        return false;
    }
    tree.bubble_layout(node);
    tree.invalidate_caches_upwards(node);
    true
}

// 消费者：`impl NodeTree::mark_subtree_paint` 的薄委托（外部调用点见该方法文档）。
/// 整棵子树标 `PAINT | A11Y`（并清 `REUSABLE`）—— 重挂后绘制桶顺序与 a11y 父链都可能变化
/// （消费者：`append_child`；`A11Y` 是 P11 追加，见 `design/phase-11-a11y.md` §6.4）。
pub(crate) fn mark_subtree_paint(tree: &mut NodeTree, node: NodeId) {
    let ids = tree.collect_subtree_ids(node);
    for id in ids {
        let _ = tree.touch(id, DirtyFlags::PAINT | DirtyFlags::A11Y);
    }
}

// 消费者：`impl NodeTree::clear_paint` 的薄委托（外部调用点见该方法文档）。
/// 消费 `PAINT`（`refresh_styles` 求值完一个节点后调用）。
pub(crate) fn clear_paint(tree: &mut NodeTree, node: NodeId) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.dirty.remove(DirtyFlags::PAINT);
            true
        }
        None => false,
    }
}

// 消费者：`impl NodeTree::mark_moved_a11y` 的薄委托（外部调用点见该方法文档）。
/// 布局 post-pass：把「布局盒与上一帧不同」的节点标 `A11Y`，返回条数。
///
/// `set_size` / `set_scale` 只标根，子树节点的 `layout.bounds` 由布局 pass 写入而不经过
/// `mark_*` —— 不在这里补标，窗口 resize 后子孙的 a11y bounds 就是旧值（P11 审查 #1）。
/// `prev_layout` 只在 `settle_frame` 前进，所以本函数在「布局后、提交前」是准确的。
pub(crate) fn mark_moved_a11y(tree: &mut NodeTree) -> usize {
    let mut moved: SmallVec<[NodeId; 8]> = SmallVec::new();
    for node in &tree.order {
        if let Some(target) = tree.arena.get(*node)
            && target.layout != target.prev_layout
        {
            moved.push(*node);
        }
    }
    let count = moved.len();
    for node in moved {
        let _ = tree.touch(node, DirtyFlags::A11Y);
    }
    count
}

// 消费者：`impl NodeTree::clear_a11y` 的薄委托（外部调用点见该方法文档）。
/// 消费 `A11Y | ORDER`（a11y pass 交付完一笔后调用；`P11` 起 `ORDER` 终于有清除者）。
pub(crate) fn clear_a11y(tree: &mut NodeTree, node: NodeId) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    let had = target.dirty.contains(DirtyFlags::A11Y);
    target.dirty.remove(DirtyFlags::A11Y | DirtyFlags::ORDER);
    if had {
        tree.a11y_dirty = tree.a11y_dirty.saturating_sub(1);
    }
    true
}

// 消费者：`impl NodeTree::a11y_dirty` 的薄委托（外部调用点见该方法文档）。
/// 带 `A11Y` 位的节点数（O(1)；平台层每轮用它判「有没有 a11y 工作」）。
pub(crate) fn a11y_dirty(tree: &NodeTree) -> u32 {
    tree.a11y_dirty
}

// 消费者：`impl NodeTree::touch` 的薄委托（外部调用点见该方法文档）。
/// 标记 + damage 并集 + 清 `REUSABLE`（唯一的写入口，§9.3）。
pub(crate) fn touch(tree: &mut NodeTree, node: NodeId, flags: DirtyFlags) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    let had_a11y = target.dirty.contains(DirtyFlags::A11Y);
    target.dirty.insert(flags);
    if flags.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
        target.dirty.remove(DirtyFlags::REUSABLE);
        target.accumulate_damage();
    }
    // `A11Y` 是累积位（`settle_frame` 不清它），计数必须与位同步（P11 S-108）。
    if flags.contains(DirtyFlags::A11Y) && !had_a11y {
        tree.a11y_dirty = tree.a11y_dirty.saturating_add(1);
    }
    true
}

// 消费者：`impl NodeTree::touch_layout_only` 的薄委托（外部调用点见该方法文档）。
/// 只标 `LAYOUT`（不并 damage、不清 `REUSABLE`）—— `bubble_layout` 专用。
///
/// 祖先需要知道「要重新跑布局」，但**它自己的绘制输出没有变**：真正的像素变化是
/// 移动/改尺寸的那个节点，`place_subtree` 在写新 `bounds` 时会精确地把
/// `old ∪ new` 并进它的 damage 并清掉它的 `REUSABLE`。
///
/// 旧实现让冒泡复用 `touch(LAYOUT)`，于是每个祖先都把**自己的盒子**并进 damage，
/// 一路到根 ⇒ 根盒 = 整个窗口 ⇒ 文本宽度一变就整帧重绘（价格图每 tick 全帧重画的
/// 直接原因，P2-3.2）。
pub(crate) fn touch_layout_only(tree: &mut NodeTree, node: NodeId) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    target.dirty.insert(DirtyFlags::LAYOUT);
    true
}

// 消费者：`impl NodeTree::any_node` 的薄委托（外部调用点见该方法文档）。
/// 遍历全部存活节点并测试谓词（消费者：`StylePassReport` 的 `layout_dirty`/`paint_dirty`）。
pub(crate) fn any_node(tree: &NodeTree, mut predicate: impl FnMut(&Node) -> bool) -> bool {
    tree.order
        .iter()
        .any(|id| tree.arena.get(*id).is_some_and(&mut predicate))
}

// 消费者：`impl NodeTree::clear_all_dirty_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：清掉全部节点的失效位（让「标了什么」可被观察）。
#[cfg(test)]
pub(crate) fn clear_all_dirty_for_test(tree: &mut NodeTree) {
    for id in &tree.order {
        if let Some(target) = tree.arena.get_mut(*id) {
            target.dirty = DirtyFlags::empty();
        }
    }
}

// 消费者：`impl NodeTree::set_dirty_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：直接设置失效位。
#[cfg(test)]
pub(crate) fn set_dirty_for_test(tree: &mut NodeTree, node: NodeId, flags: DirtyFlags) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.dirty = flags;
            true
        }
        None => false,
    }
}

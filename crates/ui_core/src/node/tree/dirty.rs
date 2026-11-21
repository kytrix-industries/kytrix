//! 脏位：`mark_*` 系列、向上冒泡、反向集合的维护与消费。
//!
//! 位语义表见 [`crate::node::dirty`] 的模块文档：每个位只有一个写者集合、一个消费者、
//! 一个清除点。本模块只做"登记 + 冒泡"，**不**在这里清除 `LAYOUT`（taffy 布局写回时清）
//! 或 `PAINT`（paint pass 消费时清）。
//!
//! 反向集合（`dirty_layout` / `dirty_paint` / `dirty_a11y` / `dirty_bind` / 帧尾结算集合）
//! 要么在 `touch` 里同步登记（本模块），要么由 `place_subtree`（布局移动）登记；所有门禁
//! 只读集合的 `is_empty()` / `len()`，因此是 O(1)。

use crate::node::{dirty::DirtyFlags, id::NodeId, tree::NodeTree};

// 消费者：`impl NodeTree::mark_bind` 的薄委托（外部调用点见该方法文档）。
/// 标 `BIND`：动态属性（`bind_style` / `bind_text` / `bind_a11y`）需要重算。
///
/// 唯一消费者是 `App::refresh_styles`；它**不**产生像素、不清 `REUSABLE`（R03-9：BIND 与 PAINT
/// 必须是两条独立通道，否则 `refresh_styles` 会吞掉绘制请求）。
pub(crate) fn mark_bind(tree: &mut NodeTree, node: NodeId) -> bool {
    tree.touch(node, DirtyFlags::BIND)
}

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
/// 结构变更：标 `LAYOUT|PAINT|A11Y` 并向上冒泡，同时清 `node..root` 的 taffy 缓存。
///
/// 旧的 `ORDER` 位已删除（R03-13）：a11y 的结构重发由 `A11Y` 位 + 祖先链承担（`adapter.rs`），
/// 绘制序由 `children` 的 DFS 遍历直接决定，不需要任何"序变化"位。
pub(crate) fn mark_structure(tree: &mut NodeTree, node: NodeId) -> bool {
    if !tree.touch(
        node,
        DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
    ) {
        return false;
    }
    // A1：结构真的变了 ⇒ 下一次 paint 必须走全量路径（绘制序成员 / 滚动槽编号都可能变）。
    tree.structure_epoch = tree.structure_epoch.wrapping_add(1);
    tree.bubble_layout(node);
    tree.invalidate_caches_upwards(node);
    true
}

// 消费者：`impl NodeTree::mark_subtree_paint` 的薄委托（外部调用点见该方法文档）。
/// 整棵子树标 `PAINT | A11Y`（并清 `REUSABLE`）—— 重挂后绘制顺序与 a11y 父链都可能变化
/// （消费者：`append_child`；`A11Y` 是 P11 追加）。
pub(crate) fn mark_subtree_paint(tree: &mut NodeTree, node: NodeId) -> usize {
    let mut ids: Vec<NodeId> = Vec::new();
    tree.for_each_subtree(node, |id| ids.push(id));
    let mut touched = 0;
    for id in ids {
        if tree.touch(id, DirtyFlags::PAINT | DirtyFlags::A11Y) {
            touched += 1;
        }
    }
    touched
}

// 消费者：`impl NodeTree::mark_subtree_a11y` 的薄委托（外部调用点见该方法文档）。
/// 整棵子树标 `A11Y`（不冒泡、**不产生像素工作**：无 damage、不清 `REUSABLE`）。
///
/// 用于「引擎内部状态变化让**后代**的 a11y 语义整体失效」。三个消费者：
/// - `a11y::tree::write_a11y_patch`（`bind_a11y` 的两个写点 + `App::set_a11y_patch`）：
///   `role` / `hidden` 变化改变后代的推导角色 / 领养 / 隐藏继承；
/// - `app::style_pass::refresh_styles`：`visible` / `interactive` 变化改变后代的隐式
///   `Focus` / `Click` / `disabled` 能力；
/// - `app::virtual_list::set_virtual_items`：条目数变化让表格/树的行数、行号、集合位置等
///   派生字段整体失效（E9c：变化点只标行窗口 + 物理祖先链 ⇒ 表头行/单元格等兄弟分支不会
///   重进增量笔，AT 会长期读到旧总数）。
///
/// 只标 `A11Y`：这些派生的是快照字段，不是布局/像素输入。
pub(crate) fn mark_subtree_a11y(tree: &mut NodeTree, node: NodeId) -> usize {
    let mut ids: Vec<NodeId> = Vec::new();
    tree.for_each_subtree(node, |id| ids.push(id));
    let mut touched = 0;
    for id in ids {
        if tree.touch(id, DirtyFlags::A11Y) {
            touched += 1;
        }
    }
    touched
}

// 消费者：`impl NodeTree::clear_bind` 的薄委托（外部调用点见该方法文档）。
/// 消费 `BIND`（`refresh_styles` 求值完一个节点后调用：唯一清除点）。
pub(crate) fn clear_bind(tree: &mut NodeTree, node: NodeId) -> bool {
    let present = match tree.arena.get_mut(node) {
        Some(target) => {
            target.dirty.remove(DirtyFlags::BIND);
            true
        }
        None => false,
    };
    tree.dirty_bind.remove(node);
    present
}

// 消费者：`impl NodeTree::clear_paint` 的薄委托（外部调用点见该方法文档）。
/// 消费 `PAINT`（paint pass 重建 / 隐藏完一个节点后调用：唯一清除点）。
pub(crate) fn clear_paint(tree: &mut NodeTree, node: NodeId) -> bool {
    let present = match tree.arena.get_mut(node) {
        Some(target) => {
            target.dirty.remove(DirtyFlags::PAINT);
            true
        }
        None => false,
    };
    tree.dirty_paint.remove(node);
    present
}

// 消费者：`impl NodeTree::mark_moved_a11y` 的薄委托（外部调用点见该方法文档）。
/// 布局 post-pass：把「本轮布局位置/尺寸真的变了」的节点标 `A11Y`，返回条数。
///
/// `set_size` / `set_scale` 只标根，子树节点的 `layout.bounds` 由布局 pass 写入而不经过
/// `mark_*` —— 由 `place_subtree` 在写新盒时登记，这里只是消费。旧实现为此每轮布局后扫全树
/// （R03-20），现在成本是 O(移动节点数)。
pub(crate) fn mark_moved_a11y(tree: &mut NodeTree) -> usize {
    let moved = tree.take_moved();
    let count = moved.len();
    for node in moved.iter().copied() {
        let _ = tree.touch(node, DirtyFlags::A11Y);
    }
    tree.recycle(moved);
    count
}

// 消费者：`impl NodeTree::clear_a11y` 的薄委托（外部调用点见该方法文档）。
/// 消费 `A11Y`（a11y pass 交付完一笔后调用：唯一清除点）。
pub(crate) fn clear_a11y(tree: &mut NodeTree, node: NodeId) -> bool {
    let present = match tree.arena.get_mut(node) {
        Some(target) => {
            target.dirty.remove(DirtyFlags::A11Y);
            true
        }
        None => false,
    };
    tree.dirty_a11y.remove(node);
    present
}

// 消费者：`impl NodeTree::a11y_dirty` 的薄委托（外部调用点见该方法文档）。
/// 带 `A11Y` 位的节点数（O(1)；平台层每轮用它判「有没有 a11y 工作」）。
pub(crate) fn a11y_dirty(tree: &NodeTree) -> usize {
    tree.dirty_a11y.len()
}

// 消费者：`impl NodeTree::touch` 的薄委托（外部调用点见该方法文档）。
/// 标记 + damage 并集 + 清 `REUSABLE` + 反向集合同步（唯一的写入口，§9.3）。
pub(crate) fn touch(tree: &mut NodeTree, node: NodeId, flags: DirtyFlags) -> bool {
    let NodeTree {
        arena,
        dirty_bind,
        dirty_layout,
        dirty_paint,
        dirty_a11y,
        dirty_settle,
        ..
    } = tree;
    let Some(target) = arena.get_mut(node) else {
        return false;
    };
    target.dirty.insert(flags);
    if flags.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
        target.dirty.remove(DirtyFlags::REUSABLE);
        target.accumulate_damage();
        dirty_settle.insert(node);
    }
    if flags.contains(DirtyFlags::BIND) {
        dirty_bind.insert(node);
    }
    if flags.contains(DirtyFlags::LAYOUT) {
        dirty_layout.insert(node);
    }
    if flags.contains(DirtyFlags::PAINT) {
        dirty_paint.insert(node);
    }
    if flags.contains(DirtyFlags::A11Y) {
        dirty_a11y.insert(node);
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
    tree.dirty_layout.insert(node);
    true
}

// 消费者：`impl NodeTree::take_dirty_bind` 的薄委托（外部调用点见该方法文档）。
/// 快照 `BIND` 集合（插入序）与复用缓冲；消费方必须 `recycle` 归还缓冲。
///
/// **不**清空集合：每个节点在 `refresh_styles` 里被处理后由 `clear_bind` 单独出集合。
/// 这样应用闭包 panic 展开时，未处理的节点仍留在集合里（下一次 pass 继续），而不会像
/// "整体 drain" 那样把工作丢掉（R03-12 的 panic-safety）。
pub(crate) fn take_dirty_bind(tree: &mut NodeTree) -> Vec<NodeId> {
    let NodeTree {
        dirty_scratch,
        dirty_bind,
        ..
    } = tree;
    let mut out = std::mem::take(dirty_scratch);
    dirty_bind.snapshot_into(&mut out);
    #[cfg(feature = "test-support")]
    for node in &out {
        tree.note_scanned(*node);
    }
    out
}

// 消费者：`impl NodeTree::take_settle` 的薄委托（外部调用点见该方法文档）。
/// 取走帧尾结算集合（`settle_frame` 的唯一输入）。
pub(crate) fn take_settle(tree: &mut NodeTree) -> Vec<NodeId> {
    let NodeTree {
        dirty_scratch,
        dirty_settle,
        ..
    } = tree;
    let mut out = std::mem::take(dirty_scratch);
    dirty_settle.take_into(&mut out);
    #[cfg(feature = "test-support")]
    for node in &out {
        tree.note_scanned(*node);
    }
    out
}

// 消费者：`impl NodeTree::take_a11y` 的薄委托（外部调用点见该方法文档）。
/// 取走 `A11Y` 集合（a11y 增量交付的候选集）。
pub(crate) fn take_a11y(tree: &mut NodeTree) -> Vec<NodeId> {
    let NodeTree {
        dirty_scratch,
        dirty_a11y,
        ..
    } = tree;
    let mut out = std::mem::take(dirty_scratch);
    dirty_a11y.take_into(&mut out);
    #[cfg(feature = "test-support")]
    for node in &out {
        tree.note_scanned(*node);
    }
    out
}

// 消费者：`impl NodeTree::take_moved` 的薄委托（外部调用点见该方法文档）。
/// 取走「本轮布局移动过」的节点（`mark_moved_a11y` 的唯一输入）。
pub(crate) fn take_moved(tree: &mut NodeTree) -> Vec<NodeId> {
    let NodeTree {
        dirty_scratch,
        moved_nodes,
        ..
    } = tree;
    let mut out = std::mem::take(dirty_scratch);
    moved_nodes.take_into(&mut out);
    #[cfg(feature = "test-support")]
    for node in &out {
        tree.note_scanned(*node);
    }
    out
}

// 消费者：`impl NodeTree::clear_all_dirty_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：清掉全部节点的失效位与全部反向集合（让「标了什么」可被观察）。
#[cfg(test)]
pub(crate) fn clear_all_dirty_for_test(tree: &mut NodeTree) {
    let mut ids: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|id| ids.push(id));
    for id in ids {
        if let Some(target) = tree.arena.get_mut(id) {
            target.dirty = DirtyFlags::empty();
        }
    }
    clear_sets(tree);
}

// 消费者：`impl NodeTree::set_dirty_for_test` 的薄委托（外部调用点见该方法文档）。
/// 仅测试：直接设置失效位（反向集合同步，便于后续 mark_* 的观察）。
#[cfg(test)]
pub(crate) fn set_dirty_for_test(tree: &mut NodeTree, node: NodeId, flags: DirtyFlags) -> bool {
    if let Some(target) = tree.arena.get_mut(node) {
        target.dirty = flags;
    } else {
        return false;
    }
    clear_sets(tree);
    sync_set(tree, node, flags);
    true
}

/// 仅测试：清空全部反向集合。
#[cfg(test)]
fn clear_sets(tree: &mut NodeTree) {
    tree.dirty_bind.clear();
    tree.dirty_layout.clear();
    tree.dirty_paint.clear();
    tree.dirty_a11y.clear();
    tree.dirty_settle.clear();
    tree.moved_nodes.clear();
}

/// 仅测试：按位把节点登记进对应集合。
#[cfg(test)]
fn sync_set(tree: &mut NodeTree, node: NodeId, flags: DirtyFlags) {
    if flags.contains(DirtyFlags::BIND) {
        tree.dirty_bind.insert(node);
    }
    if flags.contains(DirtyFlags::LAYOUT) {
        tree.dirty_layout.insert(node);
    }
    if flags.contains(DirtyFlags::PAINT) {
        tree.dirty_paint.insert(node);
    }
    if flags.contains(DirtyFlags::A11Y) {
        tree.dirty_a11y.insert(node);
    }
    if flags.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT) {
        tree.dirty_settle.insert(node);
    }
}

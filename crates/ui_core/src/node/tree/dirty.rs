use crate::node::{dirty::DirtyFlags, id::NodeId, tree::NodeTree};

pub(crate) fn mark_bind(tree: &mut NodeTree, node: NodeId) -> bool {
    tree.touch(node, DirtyFlags::BIND)
}

pub(crate) fn mark_paint(tree: &mut NodeTree, node: NodeId) -> bool {
    tree.touch(node, DirtyFlags::PAINT)
}

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

pub(crate) fn mark_a11y(tree: &mut NodeTree, node: NodeId) -> bool {
    tree.touch(node, DirtyFlags::A11Y)
}

pub(crate) fn mark_structure(tree: &mut NodeTree, node: NodeId) -> bool {
    if !tree.touch(
        node,
        DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
    ) {
        return false;
    }
    tree.structure_epoch = tree.structure_epoch.wrapping_add(1);
    tree.bubble_layout(node);
    tree.invalidate_caches_upwards(node);
    true
}

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

pub(crate) fn mark_moved_a11y(tree: &mut NodeTree) -> usize {
    let moved = tree.take_moved();
    let count = moved.len();
    for node in moved.iter().copied() {
        let _ = tree.touch(node, DirtyFlags::A11Y);
    }
    tree.recycle(moved);
    count
}

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

pub(crate) fn a11y_dirty(tree: &NodeTree) -> usize {
    tree.dirty_a11y.len()
}

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

pub(crate) fn touch_layout_only(tree: &mut NodeTree, node: NodeId) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    target.dirty.insert(DirtyFlags::LAYOUT);
    tree.dirty_layout.insert(node);
    true
}

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

#[cfg(test)]
fn clear_sets(tree: &mut NodeTree) {
    tree.dirty_bind.clear();
    tree.dirty_layout.clear();
    tree.dirty_paint.clear();
    tree.dirty_a11y.clear();
    tree.dirty_settle.clear();
    tree.moved_nodes.clear();
}

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

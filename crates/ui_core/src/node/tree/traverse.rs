use smallvec::SmallVec;

use super::NodeTree;
use crate::node::id::NodeId;

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

pub(crate) fn dfs_ids(tree: &NodeTree) -> Vec<NodeId> {
    let mut ids = Vec::with_capacity(tree.arena.len());
    for_each_dfs(tree, |id| ids.push(id));
    ids
}

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

pub(crate) fn is_visible_locally(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena.get(node).is_some_and(|target| {
        !matches!(target.style.inner().display, taffy::style::Display::None)
            && target.style_patch().visible != Some(false)
    })
}

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

pub(crate) fn hoisted_clear(tree: &mut NodeTree, node: NodeId) {
    if let Some(list) = tree.hoisted.get_mut(&node) {
        list.clear();
    }
}

pub(crate) fn hoisted_push(tree: &mut NodeTree, node: NodeId, hoisted: &[NodeId]) {
    tree.hoisted
        .entry(node)
        .or_default()
        .extend_from_slice(hoisted);
}

pub(crate) fn note_scroll_container(tree: &mut NodeTree, node: NodeId) {
    if !tree.scroll_containers.contains(&node) {
        tree.scroll_containers.push(node);
    }
    tree.scroll_reg_epoch = tree.scroll_reg_epoch.wrapping_add(1);
}

pub(crate) fn note_virtual_container(tree: &mut NodeTree, node: NodeId) {
    if !tree.virtual_containers.contains(&node) {
        tree.virtual_containers.push(node);
    }
}

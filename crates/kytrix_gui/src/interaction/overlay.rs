//! 浮层：层级（root 的末尾子节点）、外部点击命中、定位求值。
//!
//! 浮层节点由 `App::create_overlay` 创建（`append_child(root, …)`），因此绘制序在 `order` 末尾、命中优先级
//! 由 `hit_test` 的逆序遍历天然给出（`design/phase-10-interaction.md` §9）。

use crate::{
    interaction::{OverlayDismiss, OverlaySlot},
    node::{id::NodeId, tree::NodeTree},
};

/// `node` 是否是 `ancestor` 本身或其后代。
#[must_use]
pub(crate) fn is_in_subtree(tree: &NodeTree, node: NodeId, ancestor: NodeId) -> bool {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    false
}

/// 最上层「点击外部即关闭」的浮层：`hit` 不在它的子树里 ⇒ 返回它（调用方负责关闭并消费该次按下）。
///
/// 返回 `None` 表示：没有浮层 / 只有 `Explicit` 浮层 / 命中点落在最上层可关闭浮层内部。
#[must_use]
pub(crate) fn outside_click_target(
    tree: &NodeTree,
    root: NodeId,
    hit: Option<NodeId>,
) -> Option<(NodeId, OverlaySlot)> {
    let children = tree.get(root).ok()?.children();
    for child in children.iter().rev() {
        let Some(slot) = tree.overlay_slot(*child) else {
            continue;
        };
        if slot.dismiss != OverlayDismiss::OutsideClick {
            continue;
        }
        if hit.is_some_and(|node| is_in_subtree(tree, node, *child)) {
            return None;
        }
        return Some((*child, slot));
    }
    None
}

/// 最上层「可被 `Dismiss` 关闭」的浮层（任意 `dismiss` 策略）。
#[must_use]
pub(crate) fn topmost(tree: &NodeTree, root: NodeId) -> Option<(NodeId, OverlaySlot)> {
    let children = tree.get(root).ok()?.children();
    children
        .iter()
        .rev()
        .find_map(|child| tree.overlay_slot(*child).map(|slot| (*child, slot)))
}

#[cfg(test)]
mod tests {
    use super::{is_in_subtree, outside_click_target, topmost};
    use crate::{
        interaction::{OverlayDismiss, OverlaySlot},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    };

    fn overlay(tree: &mut NodeTree, root: NodeId, dismiss: OverlayDismiss) -> NodeId {
        let node = NodeBuilder::boxed().build(tree, root).expect("mount");
        tree.ensure_interaction(node).expect("exists").overlay = Some(OverlaySlot { dismiss });
        node
    }

    #[test]
    fn subtree_check_walks_parents() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        let inner = NodeBuilder::boxed().build(&mut tree, outer).expect("mount");

        assert!(is_in_subtree(&tree, inner, outer));
        assert!(is_in_subtree(&tree, outer, outer));
        assert!(!is_in_subtree(&tree, outer, inner));
        assert!(is_in_subtree(&tree, outer, root));
    }

    #[test]
    fn outside_click_targets_the_topmost_dismissible_overlay() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let lower = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let upper = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let background = NodeBuilder::boxed().build(&mut tree, root).expect("mount");

        assert_eq!(
            outside_click_target(&tree, root, Some(background)).map(|(node, _)| node),
            Some(upper)
        );
        assert_eq!(
            outside_click_target(&tree, root, Some(lower)).map(|(node, _)| node),
            Some(upper),
            "命中在下层浮层里 ⇒ 仍然先关最上层"
        );
        assert_eq!(
            outside_click_target(&tree, root, Some(upper)),
            None,
            "点在浮层内部不关"
        );
    }

    #[test]
    fn explicit_overlays_never_close_on_outside_click() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = overlay(&mut tree, root, OverlayDismiss::Explicit);
        let background = NodeBuilder::boxed().build(&mut tree, root).expect("mount");

        assert_eq!(outside_click_target(&tree, root, Some(background)), None);
        assert!(topmost(&tree, root).is_some(), "Dismiss 仍可关它");
    }

    #[test]
    fn a_hit_inside_the_topmost_overlay_keeps_everything_open() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let panel = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let child = NodeBuilder::boxed().build(&mut tree, panel).expect("mount");

        assert_eq!(outside_click_target(&tree, root, Some(child)), None);
    }
}

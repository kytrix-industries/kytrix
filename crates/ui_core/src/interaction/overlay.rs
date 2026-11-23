use crate::{
    interaction::{OverlayDismiss, OverlaySlot},
    node::{id::NodeId, tree::NodeTree},
};

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

const MAX_OVERLAY_DEPTH: usize = 32;

fn enclosing_overlay(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if tree.overlay_slot(current).is_some() {
            return Some(current);
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    None
}

#[must_use]
pub(crate) fn in_modal_scope(tree: &NodeTree, node: NodeId, scope: Option<NodeId>) -> bool {
    let Some(scope) = scope else {
        return true;
    };
    let mut cursor = node;
    for _ in 0..MAX_OVERLAY_DEPTH {
        if is_in_subtree(tree, cursor, scope) {
            return true;
        }
        let Some(overlay) = enclosing_overlay(tree, cursor) else {
            return false;
        };
        let Some(opener) = tree.overlay_slot(overlay).and_then(|slot| slot.opener) else {
            return false;
        };
        if tree.get(opener).is_err() {
            return false;
        }
        cursor = opener;
    }
    false
}

#[must_use]
pub(crate) fn outside_click_target(
    tree: &NodeTree,
    root: NodeId,
    hit: Option<NodeId>,
) -> Option<(NodeId, OverlaySlot)> {
    let children = tree.get(root).ok()?.children();
    let top = children
        .iter()
        .rev()
        .find_map(|child| tree.overlay_slot(*child).map(|slot| (*child, slot)));
    let (topmost, slot) = top?;
    if slot.dismiss != OverlayDismiss::OutsideClick {
        return None;
    }
    if hit.is_some_and(|node| is_in_subtree(tree, node, topmost)) {
        return None;
    }
    Some((topmost, slot))
}

#[must_use]
pub(crate) fn topmost(tree: &NodeTree, root: NodeId) -> Option<(NodeId, OverlaySlot)> {
    let children = tree.get(root).ok()?.children();
    children
        .iter()
        .rev()
        .find_map(|child| tree.overlay_slot(*child).map(|slot| (*child, slot)))
}

#[must_use]
pub(crate) fn focus_scope_root(tree: &NodeTree, root: NodeId) -> Option<NodeId> {
    let children = tree.get(root).ok()?.children();
    children.iter().rev().find_map(|child| {
        tree.overlay_slot(*child)
            .filter(|slot| slot.trap_focus)
            .map(|_| *child)
    })
}

#[cfg(test)]
mod tests {
    use super::{focus_scope_root, in_modal_scope, is_in_subtree, outside_click_target, topmost};
    use crate::{
        interaction::{OverlayDismiss, OverlayPlacement, OverlaySlot},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    };

    fn overlay_slot(dismiss: OverlayDismiss) -> OverlaySlot {
        OverlaySlot {
            dismiss,
            placement: OverlayPlacement::Point(crate::math::Point::ZERO),
            trap_focus: false,
            opener: None,
            restore_focus: false,
        }
    }

    fn overlay(tree: &mut NodeTree, root: NodeId, dismiss: OverlayDismiss) -> NodeId {
        let node = NodeBuilder::boxed().build(tree, root).expect("mount");
        tree.ensure_interaction(node).expect("exists").overlay = Some(overlay_slot(dismiss));
        node
    }

    fn overlay_with_opener(
        tree: &mut NodeTree,
        root: NodeId,
        dismiss: OverlayDismiss,
        opener: Option<NodeId>,
    ) -> NodeId {
        let node = NodeBuilder::boxed().build(tree, root).expect("mount");
        let slot = OverlaySlot {
            opener,
            ..overlay_slot(dismiss)
        };
        tree.ensure_interaction(node).expect("exists").overlay = Some(slot);
        node
    }

    fn modal_overlay(tree: &mut NodeTree, root: NodeId) -> NodeId {
        let node = NodeBuilder::boxed().build(tree, root).expect("mount");
        let slot = OverlaySlot {
            trap_focus: true,
            ..overlay_slot(OverlayDismiss::Explicit)
        };
        tree.ensure_interaction(node).expect("exists").overlay = Some(slot);
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
    fn an_explicit_overlay_blocks_outside_click_for_lower_overlays() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _lower = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let _upper = overlay(&mut tree, root, OverlayDismiss::Explicit);
        let background = NodeBuilder::boxed().build(&mut tree, root).expect("mount");

        assert_eq!(
            outside_click_target(&tree, root, Some(background)),
            None,
            "顶层是 Explicit ⇒ 点击外部不得关闭任何下层浮层"
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

    #[test]
    fn the_topmost_modal_overlay_owns_the_focus_scope() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let lower = modal_overlay(&mut tree, root);
        let _plain = overlay(&mut tree, root, OverlayDismiss::OutsideClick);

        assert_eq!(
            focus_scope_root(&tree, root),
            Some(lower),
            "非模态浮层不影响作用域，仍然是最上层模态浮层"
        );

        let upper = modal_overlay(&mut tree, root);
        assert_eq!(focus_scope_root(&tree, root), Some(upper), "栈顶说了算");
    }

    #[test]
    fn a_window_without_modals_has_no_focus_scope() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _plain = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let _background = NodeBuilder::boxed().build(&mut tree, root).expect("mount");

        assert_eq!(focus_scope_root(&tree, root), None);
    }

    #[test]
    fn a_popup_opened_from_inside_the_modal_stays_in_scope() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let modal = modal_overlay(&mut tree, root);
        let item = NodeBuilder::boxed().build(&mut tree, modal).expect("mount");
        let background = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        let menu = overlay_with_opener(&mut tree, root, OverlayDismiss::OutsideClick, Some(item));
        let menu_item = NodeBuilder::boxed().build(&mut tree, menu).expect("mount");

        let scope = focus_scope_root(&tree, root);
        assert_eq!(scope, Some(modal));
        assert!(in_modal_scope(&tree, menu_item, scope), "菜单项必须可交互");
        assert!(in_modal_scope(&tree, menu, scope));
        assert!(in_modal_scope(&tree, item, scope));
        assert!(
            !in_modal_scope(&tree, background, scope),
            "模态外的背景仍在作用域外"
        );
        let stray = overlay_with_opener(
            &mut tree,
            root,
            OverlayDismiss::OutsideClick,
            Some(background),
        );
        let stray_item = NodeBuilder::boxed().build(&mut tree, stray).expect("mount");
        assert!(!in_modal_scope(&tree, stray_item, scope));
        let orphan = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let orphan_item = NodeBuilder::boxed()
            .build(&mut tree, orphan)
            .expect("mount");
        assert!(!in_modal_scope(&tree, orphan_item, scope));
    }

    #[test]
    fn nested_popups_follow_the_opener_chain_back_into_the_modal() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let modal = modal_overlay(&mut tree, root);
        let item = NodeBuilder::boxed().build(&mut tree, modal).expect("mount");
        let menu = overlay_with_opener(&mut tree, root, OverlayDismiss::OutsideClick, Some(item));
        let submenu_entry = NodeBuilder::boxed().build(&mut tree, menu).expect("mount");
        let submenu = overlay_with_opener(
            &mut tree,
            root,
            OverlayDismiss::OutsideClick,
            Some(submenu_entry),
        );
        let submenu_item = NodeBuilder::boxed()
            .build(&mut tree, submenu)
            .expect("mount");

        let scope = focus_scope_root(&tree, root);
        assert!(
            in_modal_scope(&tree, submenu_item, scope),
            "子菜单经 opener 链仍属于模态"
        );
    }

    #[test]
    fn a_cyclic_opener_chain_terminates() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let modal = modal_overlay(&mut tree, root);
        let menu = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let child = NodeBuilder::boxed().build(&mut tree, menu).expect("mount");
        tree.ensure_interaction(menu).expect("exists").overlay = Some(OverlaySlot {
            opener: Some(child),
            ..overlay_slot(OverlayDismiss::OutsideClick)
        });

        let scope = focus_scope_root(&tree, root);
        assert_eq!(scope, Some(modal), "环上的浮层不得改变作用域根");
        assert!(
            !in_modal_scope(&tree, child, scope),
            "环上的浮层不属于模态，且判定必须终止"
        );
    }

    #[test]
    fn without_a_scope_every_node_is_in_scope() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("mount");

        assert!(in_modal_scope(&tree, node, None));
    }
}

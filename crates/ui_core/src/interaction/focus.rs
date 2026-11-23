use crate::{
    event::Action,
    node::{id::NodeId, tree::NodeTree},
};

#[must_use]
pub(crate) fn accepts_action(tree: &NodeTree, node: NodeId) -> bool {
    tree.interaction(node)
        .is_some_and(|slot| slot.action.is_some())
        && tree.is_effectively_interactive(node)
}

#[must_use]
pub(crate) fn focusable(tree: &NodeTree, node: NodeId) -> bool {
    if !tree.is_effectively_visible(node)
        || !tree.is_effectively_interactive(node)
        || !tree.get(node).is_ok_and(|target| target.tab_stop())
    {
        return false;
    }
    accepts_action(tree, node) || tree.is_explicitly_interactive(node)
}

#[must_use]
pub(crate) fn in_scope(tree: &NodeTree, node: NodeId, scope: Option<NodeId>) -> bool {
    crate::interaction::overlay::in_modal_scope(tree, node, scope)
}

#[must_use]
pub(crate) fn focusable_in_scope(tree: &NodeTree, node: NodeId, scope: Option<NodeId>) -> bool {
    in_scope(tree, node, scope) && focusable(tree, node)
}

pub(crate) fn next_focusable(
    tree: &NodeTree,
    from: Option<NodeId>,
    backwards: bool,
    scope: Option<NodeId>,
) -> Option<NodeId> {
    let order = tree.dfs_ids();
    let candidate = |node: &NodeId| focusable_in_scope(tree, *node, scope);
    let Some(from) = from.filter(|node| candidate(node)) else {
        return if backwards {
            order.iter().rev().copied().find(candidate)
        } else {
            order.iter().copied().find(candidate)
        };
    };
    let Some(start) = order.iter().position(|node| *node == from) else {
        return if backwards {
            order.iter().rev().copied().find(candidate)
        } else {
            order.iter().copied().find(candidate)
        };
    };
    let len = order.len();
    for step in 1..=len {
        let index = if backwards {
            (start + len - step) % len
        } else {
            (start + step) % len
        };
        let node = order[index];
        if candidate(&node) {
            return Some(node);
        }
    }
    None
}

#[must_use]
pub(crate) fn nearest_focusable(tree: &NodeTree, from: NodeId) -> Option<NodeId> {
    let mut cursor = Some(from);
    while let Some(node) = cursor {
        if focusable(tree, node) {
            return Some(node);
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }
    None
}

#[must_use]
pub(crate) fn resolve_cursor(
    tree: &NodeTree,
    hover: Option<NodeId>,
) -> crate::paint::style::Cursor {
    let mut cursor = hover;
    while let Some(node) = cursor {
        if let Ok(target) = tree.get(node)
            && let Some(shape) = target.style_patch().cursor
        {
            return shape;
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }
    crate::paint::style::Cursor::Default
}
#[must_use]
pub(crate) fn action_target(tree: &NodeTree, from: NodeId) -> Option<(NodeId, Action)> {
    let mut cursor = Some(from);
    while let Some(node) = cursor {
        if !tree.is_hittable(node) {
            return None;
        }
        if let Some(slot) = tree.interaction(node)
            && let Some(action) = slot.action.clone()
        {
            return Some((node, action));
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }
    None
}

#[must_use]
pub(crate) fn drag_target(tree: &NodeTree, from: NodeId) -> Option<(NodeId, Action)> {
    let mut cursor = Some(from);
    while let Some(node) = cursor {
        if !tree.is_hittable(node) {
            return None;
        }
        if let Some(slot) = tree.interaction(node)
            && let Some(action) = slot.drag.clone()
        {
            return Some((node, action));
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        action_target, drag_target, focusable, focusable_in_scope, in_scope, nearest_focusable,
        next_focusable,
    };
    use crate::{
        event::Action,
        interaction::overlay::focus_scope_root,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style::StylePatch,
    };

    fn plain(tree: &mut NodeTree, parent: NodeId) -> NodeId {
        NodeBuilder::boxed().build(tree, parent).expect("mount")
    }

    #[test]
    fn invisible_subtrees_are_not_focusable() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let visible = NodeBuilder::boxed()
            .on_click(Action::custom("visible"))
            .build(&mut tree, root)
            .expect("mount");
        let hidden_parent = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        let hidden_child = NodeBuilder::boxed()
            .on_click(Action::custom("hidden"))
            .build(&mut tree, hidden_parent)
            .expect("mount");
        tree.set_style_patch(
            hidden_parent,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );

        assert!(focusable(&tree, visible), "可见按钮必须可聚焦");
        assert!(
            !focusable(&tree, hidden_child),
            "不可见容器的子节点不得可聚焦"
        );
        assert_eq!(
            next_focusable(&tree, Some(visible), false, None),
            Some(visible),
            "Tab 序只含可见节点（单节点时回绕到自己）"
        );
    }

    #[test]
    fn tab_order_is_dfs_preorder_and_skips_plain_nodes() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let first = NodeBuilder::boxed()
            .on_click(Action::custom("a"))
            .build(&mut tree, root)
            .expect("mount");
        let middle = plain(&mut tree, root);
        let nested_parent = plain(&mut tree, root);
        let nested = NodeBuilder::boxed()
            .on_click(Action::custom("b"))
            .build(&mut tree, nested_parent)
            .expect("mount");

        assert!(focusable(&tree, first));
        assert!(!focusable(&tree, middle), "普通节点不可聚焦");

        let sequence: Vec<NodeId> = std::iter::successors(Some(first), |node| {
            next_focusable(&tree, Some(*node), false, None)
        })
        .take(3)
        .collect();
        assert_eq!(sequence, vec![first, nested, first], "先序 + 回绕");
    }

    #[test]
    fn tab_wraps_both_directions() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = NodeBuilder::boxed()
            .on_click(Action::custom("a"))
            .build(&mut tree, root)
            .expect("mount");
        let b = NodeBuilder::boxed()
            .on_click(Action::custom("b"))
            .build(&mut tree, root)
            .expect("mount");

        assert_eq!(
            next_focusable(&tree, Some(a), true, None),
            Some(b),
            "反向回绕"
        );
        assert_eq!(
            next_focusable(&tree, Some(b), false, None),
            Some(a),
            "正向回绕"
        );
        assert_eq!(next_focusable(&tree, None, false, None), Some(a));
        assert_eq!(next_focusable(&tree, None, true, None), Some(b));
    }

    #[test]
    fn no_candidates_yields_none() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = plain(&mut tree, root);

        assert_eq!(next_focusable(&tree, None, false, None), None);
        assert_eq!(next_focusable(&tree, None, true, None), None);
    }

    #[test]
    fn interactive_true_is_focusable_without_action() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        tree.set_style_patch_for_test(
            node,
            StylePatch {
                interactive: Some(true),
                ..StylePatch::default()
            },
        );

        assert!(focusable(&tree, node));
        assert_eq!(next_focusable(&tree, None, false, None), Some(node));
    }

    #[test]
    fn tab_stop_false_keeps_the_action_but_skips_the_tab_order() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let first = NodeBuilder::boxed()
            .on_click(Action::custom("first"))
            .build(&mut tree, root)
            .expect("mount");
        let chrome = NodeBuilder::boxed()
            .on_click(Action::custom("chrome"))
            .tab_stop(false)
            .build(&mut tree, root)
            .expect("mount");

        assert!(!focusable(&tree, chrome), "窗口控件不进入应用 Tab 序");
        assert_eq!(
            tree.interaction(chrome)
                .and_then(|slot| slot.action.clone()),
            Some(Action::custom("chrome")),
            "点击 / AT 的动作仍然保留"
        );
        assert_eq!(next_focusable(&tree, Some(first), false, None), Some(first));
    }

    #[test]
    fn disabled_node_stops_bubbling() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = NodeBuilder::boxed()
            .on_click(Action::custom("outer"))
            .build(&mut tree, root)
            .expect("mount");
        let inner = NodeBuilder::boxed()
            .on_click(Action::custom("inner"))
            .build(&mut tree, outer)
            .expect("mount");
        tree.set_style_patch_for_test(
            inner,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );

        assert!(!focusable(&tree, inner));
        assert_eq!(
            action_target(&tree, inner),
            None,
            "interactive:false 处停止"
        );
        assert_eq!(drag_target(&tree, inner), None);
    }

    #[test]
    fn nearest_focusable_walks_up() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed()
            .on_click(Action::custom("ok"))
            .build(&mut tree, root)
            .expect("mount");
        let label = plain(&mut tree, button);

        assert_eq!(nearest_focusable(&tree, label), Some(button));
        assert_eq!(nearest_focusable(&tree, root), None);
    }

    #[test]
    fn action_bubbles_to_the_first_declaration() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = NodeBuilder::boxed()
            .on_click(Action::custom("parent"))
            .build(&mut tree, root)
            .expect("mount");
        let child = plain(&mut tree, parent);

        assert_eq!(
            action_target(&tree, child).map(|(node, _)| node),
            Some(parent)
        );
        assert_eq!(
            action_target(&tree, parent).map(|(_, action)| action),
            Some(Action::custom("parent"))
        );
    }

    #[test]
    fn drag_bubbles_independently_from_click() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let grip = NodeBuilder::boxed()
            .on_click(Action::custom("click"))
            .on_drag(Action::custom("drag"))
            .build(&mut tree, root)
            .expect("mount");
        let thumb = plain(&mut tree, grip);

        assert_eq!(drag_target(&tree, thumb).map(|(node, _)| node), Some(grip));
        assert_eq!(
            action_target(&tree, thumb).map(|(node, _)| node),
            Some(grip)
        );
        assert_ne!(
            drag_target(&tree, thumb).map(|(_, action)| action),
            action_target(&tree, thumb).map(|(_, action)| action),
        );
    }

    #[test]
    fn a_modal_scope_traps_the_tab_order_inside_the_overlay() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let background = NodeBuilder::boxed()
            .on_click(Action::custom("background"))
            .build(&mut tree, root)
            .expect("mount");
        let panel = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        tree.ensure_interaction(panel).expect("exists").overlay =
            Some(crate::interaction::OverlaySlot {
                dismiss: crate::interaction::OverlayDismiss::Explicit,
                placement: crate::interaction::OverlayPlacement::Point(crate::math::Point::ZERO),
                trap_focus: true,
                opener: None,
                restore_focus: false,
            });
        let first = NodeBuilder::boxed()
            .on_click(Action::custom("first"))
            .build(&mut tree, panel)
            .expect("mount");
        let second = NodeBuilder::boxed()
            .on_click(Action::custom("second"))
            .build(&mut tree, panel)
            .expect("mount");

        let scope = focus_scope_root(&tree, root);
        assert_eq!(scope, Some(panel));

        assert_eq!(
            next_focusable(&tree, Some(first), false, scope),
            Some(second)
        );
        assert_eq!(
            next_focusable(&tree, Some(second), false, scope),
            Some(first)
        );
        assert!(!focusable_in_scope(&tree, background, scope));
        assert!(focusable_in_scope(&tree, first, scope));
        assert!(!in_scope(&tree, background, scope));
    }

    #[test]
    fn no_modal_means_no_restriction() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = NodeBuilder::boxed()
            .on_click(Action::custom("a"))
            .build(&mut tree, root)
            .expect("mount");
        let b = NodeBuilder::boxed()
            .on_click(Action::custom("b"))
            .build(&mut tree, root)
            .expect("mount");

        let scope = focus_scope_root(&tree, root);
        assert_eq!(scope, None);
        assert_eq!(next_focusable(&tree, Some(a), false, scope), Some(b));
    }
}

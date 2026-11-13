//! 焦点：可聚焦谓词、Tab 序（`order` 过滤）与焦点移动的纯逻辑。
//!
//! 状态（`focused`）在 [`crate::interaction::FocusScope`]；写入口是 `App::{set_focus, focus_next, focus_prev}`
//! （`design/phase-10-interaction.md` §6）。

use crate::{
    event::Action,
    node::{id::NodeId, tree::NodeTree},
};

/// 该节点是否接受**动作**：声明了动作且未被 `interactive: false` 禁用。
#[must_use]
pub(crate) fn accepts_action(tree: &NodeTree, node: NodeId) -> bool {
    tree.interaction(node)
        .is_some_and(|slot| slot.action.is_some())
        && tree.is_hittable(node)
}

/// 该节点是否可聚焦：接受动作，或被显式标为 `interactive: Some(true)`。
#[must_use]
pub(crate) fn focusable(tree: &NodeTree, node: NodeId) -> bool {
    if !tree.is_hittable(node) || !tree.get(node).is_ok_and(|target| target.tab_stop()) {
        return false;
    }
    accepts_action(tree, node) || tree.is_explicitly_interactive(node)
}

/// 从 `from` 出发按 `order` 找下一个可聚焦节点（`backwards` = 反向；末尾回绕）。
///
/// `from == None` ⇒ 正向取第一个 / 反向取最后一个。`order` 是 DFS 先序（P03 §11.5），因此 Tab 序 = 阅读顺序。
pub(crate) fn next_focusable(
    tree: &NodeTree,
    from: Option<NodeId>,
    backwards: bool,
) -> Option<NodeId> {
    let candidates: Vec<NodeId> = tree
        .order()
        .iter()
        .copied()
        .filter(|node| focusable(tree, *node))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let Some(from) = from else {
        return if backwards {
            candidates.last().copied()
        } else {
            candidates.first().copied()
        };
    };
    let position = candidates.iter().position(|node| *node == from);
    match position {
        Some(index) if backwards => {
            let previous = (index + candidates.len() - 1) % candidates.len();
            Some(candidates[previous])
        }
        Some(index) => Some(candidates[(index + 1) % candidates.len()]),
        // 焦点不在候选集里（被禁用/卸载）⇒ 从同一位置继续：正向取第一个、反向取最后一个。
        None => {
            if backwards {
                candidates.last().copied()
            } else {
                candidates.first().copied()
            }
        }
    }
}

/// 从命中节点向上找最近的**可聚焦**祖先（点击时的焦点落点）。
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

/// 从命中的 hover 节点向上找第一条声明了光标的节点（`Cursor::Default` = 都没有声明）。
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
/// 点击（或激活）时沿祖先链找第一条声明了动作的节点。
///
/// 「声明了动作」= `Node.interaction.action.is_some()`；`interactive == Some(false)` 处**停止**（不含该节点）。
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

/// 沿祖先链找第一条声明了 `on_drag` 的节点（`interactive: false` 处停止）。
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
    use super::{action_target, drag_target, focusable, nearest_focusable, next_focusable};
    use crate::{
        event::Action,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style::StylePatch,
    };

    fn plain(tree: &mut NodeTree, parent: NodeId) -> NodeId {
        NodeBuilder::boxed().build(tree, parent).expect("mount")
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
            next_focusable(&tree, Some(*node), false)
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

        assert_eq!(next_focusable(&tree, Some(a), true), Some(b), "反向回绕");
        assert_eq!(next_focusable(&tree, Some(b), false), Some(a), "正向回绕");
        assert_eq!(next_focusable(&tree, None, false), Some(a));
        assert_eq!(next_focusable(&tree, None, true), Some(b));
    }

    #[test]
    fn no_candidates_yields_none() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = plain(&mut tree, root);

        assert_eq!(next_focusable(&tree, None, false), None);
        assert_eq!(next_focusable(&tree, None, true), None);
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
        assert_eq!(next_focusable(&tree, None, false), Some(node));
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
        assert_eq!(next_focusable(&tree, Some(first), false), Some(first));
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
}

//! 焦点：可聚焦谓词、Tab 序（DFS 先序过滤）、焦点作用域与焦点移动的纯逻辑。
//!
//! 状态（`focused`）在 [`crate::interaction::FocusScope`]；写入口是 `App::{set_focus, focus_next, focus_prev}`
//! （`design/phase-10-interaction.md` §6）。

use crate::{
    event::Action,
    node::{id::NodeId, tree::NodeTree},
};

/// 该节点是否接受**动作**：声明了动作，且**自身与祖先**都未被 `interactive: false` 禁用。
#[must_use]
pub(crate) fn accepts_action(tree: &NodeTree, node: NodeId) -> bool {
    tree.interaction(node)
        .is_some_and(|slot| slot.action.is_some())
        && tree.is_effectively_interactive(node)
}

/// 该节点是否可聚焦：接受动作，或被显式标为 `interactive: Some(true)`。
///
/// 三道门与命中测试**同源**：
/// 1. 可见性（**含祖先**）：`visible: false` / `display: none` 按 CSS 语义隐藏整棵子树
///    （旧实现只看 `interactive` 与 `tab_stop`，于是 Tab 会落到不可见控件上、Enter 还能触发
///    隐藏的 action，P2-3.3）；
/// 2. 可交互性（**含祖先**）：任一祖先 `interactive: Some(false)` ⇒ 整棵子树不可聚焦
///    （C12：与命中测试的 `disabled` 继承、a11y 的 `disabled` 同一判据）；
/// 3. `tab_stop`：窗口控件可以点击但不进 Tab 序。
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

/// 该节点是否在当前**焦点作用域**内（`None` = 没有模态浮层，整棵树都在作用域内）。
///
/// 作用域根是最上层带 `trap_focus` 的浮层（[`crate::interaction::overlay::focus_scope_root`]）。
/// R02-12：模态浮层一开，Tab 序、`set_focus` 与指针抢焦点都只认作用域内的节点 —— 否则
/// `Explicit` 策略的「模态」在用户按 Tab / 点时形同虚设。
///
/// 「作用域内」不只是子树：模态之上打开的**弹出层**（`opener` 在模态内，含嵌套）也在作用域
/// 内（[`crate::interaction::overlay::in_modal_scope`]）—— 否则对话框里的下拉菜单里的控件
/// 无法获得焦点。
#[must_use]
pub(crate) fn in_scope(tree: &NodeTree, node: NodeId, scope: Option<NodeId>) -> bool {
    crate::interaction::overlay::in_modal_scope(tree, node, scope)
}

/// 可聚焦 **且** 在当前作用域内。
#[must_use]
pub(crate) fn focusable_in_scope(tree: &NodeTree, node: NodeId, scope: Option<NodeId>) -> bool {
    in_scope(tree, node, scope) && focusable(tree, node)
}

/// 从 `from` 出发按 DFS 先序找下一个可聚焦节点（`backwards` = 反向；末尾回绕）。
///
/// `from == None` ⇒ 正向取第一个 / 反向取最后一个。遍历序是 `children` 的 DFS 先序，因此 Tab 序 = 阅读顺序。
/// `scope` 限定候选集（模态浮层的焦点陷阱，R02-12）：`from` 不在作用域内时按「无焦点」处理。
pub(crate) fn next_focusable(
    tree: &NodeTree,
    from: Option<NodeId>,
    backwards: bool,
    scope: Option<NodeId>,
) -> Option<NodeId> {
    // Tab 序 = `children` 的 DFS 先序（阅读顺序），由 `dfs_ids` 现算；不再有全序数组。
    let order = tree.dfs_ids();
    let candidate = |node: &NodeId| focusable_in_scope(tree, *node, scope);
    // 焦点不在候选集里（禁用 / 已卸载 / 无焦点 / 在作用域外）：正向取第一个、反向取最后一个。
    let Some(from) = from.filter(|node| candidate(node)) else {
        return if backwards {
            order.iter().rev().copied().find(candidate)
        } else {
            order.iter().copied().find(candidate)
        };
    };
    let Some(start) = order.iter().position(|node| *node == from) else {
        // 焦点节点不在遍历序里（游离节点）：与「无焦点」同处理。
        return if backwards {
            order.iter().rev().copied().find(candidate)
        } else {
            order.iter().copied().find(candidate)
        };
    };
    // 单趟环形扫描（末尾回绕）：零额外分配。
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
    // 走完一整圈没有可聚焦节点。`from` 自身可聚焦时上面 `step == len` 就会返回它，
    // 走到这里说明连它都不可聚焦（理论上被 `filter` 挡掉了），确实没有候选。
    None
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

    /// P2-3.3 回归：不可见容器里的控件不能被 Tab 到。
    ///
    /// `visible: false` / `display: none` 按 CSS 语义隐藏**整棵子树**，命中测试一直是这样做的，
    /// 但焦点遍历只查了 `interactive` 与 `tab_stop` —— 于是 Tab 会落到看不见的控件上，
    /// Enter 还能触发隐藏的 action。
    #[test]
    fn invisible_subtrees_are_not_focusable() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        // 可见的按钮：Tab 必须能到。
        let visible = NodeBuilder::boxed()
            .on_click(Action::custom("visible"))
            .build(&mut tree, root)
            .expect("mount");
        // 容器 `visible: false`，里面还有一个按钮。
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
        // 从可见按钮正向 Tab：不得回绕到隐藏子节点。
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

    /// R02-12：模态浮层把 Tab 序限制在自己的子树里。
    ///
    /// 旧实现 `next_focusable` 在整个 `order()` 上环形扫描，`Explicit` 策略的「模态」在用户按
    /// Tab 时形同虚设：焦点会走到被遮挡的背景控件上。
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

        // 作用域内：Tab 只在 first/second 之间循环。
        assert_eq!(
            next_focusable(&tree, Some(first), false, scope),
            Some(second)
        );
        assert_eq!(
            next_focusable(&tree, Some(second), false, scope),
            Some(first)
        );
        // 背景按钮既不在 Tab 序里，也不能被程序聚焦。
        assert!(!focusable_in_scope(&tree, background, scope));
        assert!(focusable_in_scope(&tree, first, scope));
        assert!(!in_scope(&tree, background, scope));
    }

    /// 没有模态浮层时作用域为空：整棵树都是候选（行为与旧实现一致）。
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

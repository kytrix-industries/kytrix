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
/// 返回 `None` 表示：没有浮层 / **最上层浮层不可由此关闭**（`Explicit`）/ 命中点落在它的
/// 子树内部。
///
/// **只由最上层浮层裁决**（P2-6）：旧实现会跳过 `Explicit` 浮层继续往下找，于是点击落在
/// 顶层模态浮层之外、却被压在下面的另一个浮层关掉 —— 模态失去了「挡住下层」的意义。
#[must_use]
pub(crate) fn outside_click_target(
    tree: &NodeTree,
    root: NodeId,
    hit: Option<NodeId>,
) -> Option<(NodeId, OverlaySlot)> {
    let children = tree.get(root).ok()?.children();
    // 顶层浮层说了算：它内部 ⇒ 谁都不关；它不可由此关闭 ⇒ 谁都不关（不越过它）。
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

/// 最上层「可被 `Dismiss` 关闭」的浮层（任意 `dismiss` 策略）。
#[must_use]
pub(crate) fn topmost(tree: &NodeTree, root: NodeId) -> Option<(NodeId, OverlaySlot)> {
    let children = tree.get(root).ok()?.children();
    children
        .iter()
        .rev()
        .find_map(|child| tree.overlay_slot(*child).map(|slot| (*child, slot)))
}

/// 当前的**焦点作用域根**（R02-12）：最上层带 `trap_focus` 的浮层子树根。
///
/// 作用域是「栈顶决定一切」：模态浮层一开，键盘焦点只能在它内部循环，`Tab` 不会走到被遮挡
/// 的背景控件上，`set_focus` 也会拒绝作用域外的节点（Blink `<dialog showModal>` 的 top
/// layer + `inert` 背景、Qt 对话框焦点作用域、GTK `GtkWindow` 的 modal 都是这个语义）。
/// 非模态浮层不改变作用域。
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
    use super::{focus_scope_root, is_in_subtree, outside_click_target, topmost};
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

    /// P2-6 回归：顶层 `Explicit` 浮层必须**挡住**下层的可关闭浮层。
    ///
    /// 旧实现会跳过 `Explicit` 继续往下找，于是点击落在顶层模态浮层之外、却把压在下面的
    /// 另一个浮层关掉 —— 模态失去「挡住下层」的意义。
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

    /// R02-12：焦点作用域由**最上层的模态浮层**决定，非模态浮层不改变它。
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
}

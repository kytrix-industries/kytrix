//! 浮层：层级（root 的末尾子节点）、外部点击命中、定位求值。
//!
//! 浮层节点由 `App::create_overlay` 创建（`append_child(root, …)`），因此绘制序在 DFS 先序末尾、命中优先级
//! 由 `hit_test` 的逆序遍历天然给出。

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

/// 浮层 `opener` 链的最大深度（防御损坏的环：正常嵌套只有个位数层）。
const MAX_OVERLAY_DEPTH: usize = 32;

/// 包含 `node` 的**浮层**（从 `node` 向上找到的第一个挂 `overlay` 槽的节点）。
///
/// 浮层恒挂 root（[`crate::app::App::create_overlay`]），因此这就是「该节点属于哪个浮层」。
/// 节点本身不是浮层、也不在浮层里 ⇒ `None`。
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

/// `node` 是否在模态浮层的**交互作用域**内（R02-12）。
///
/// `scope` = 最上层模态浮层的子树根（[`focus_scope_root`]）；`None` ⇒ 没有模态，整棵树都在
/// 作用域内。
///
/// 判定 = 命中节点在模态子树内 **或** 该节点所在浮层的 `opener` 在作用域内（浮层支持嵌套，
/// `opener` 链递归）。这正是「对话框里的下拉/菜单仍属于对话框」的语义：
///
/// - Qt：模态对话框的**子窗口 / 弹出窗口**（`QWidget::isAncestorOf` 与 `QDialog` 的父子链）
///   仍在对话框的交互范围内，点它们不会被视为「点到了模态外面」；
/// - Blink / Chrome：`<dialog>` 的 top layer 之上再开的 popover/menu 仍属于该对话框的焦点
///   作用域（`inert` 只作用于对话框之外的内容）；
/// - GTK：`GtkPopover` 相对其 parent 的模态性同理。
///
/// 旧实现只认 `is_in_subtree(hit, scope)`，而 `create_overlay` 恒把浮层挂 root ⇒ 对话框里的
/// 下拉菜单必然落在作用域外、点击被整体吞掉（AR1 探针 1）。
#[must_use]
pub(crate) fn in_modal_scope(tree: &NodeTree, node: NodeId, scope: Option<NodeId>) -> bool {
    let Some(scope) = scope else {
        return true;
    };
    let mut cursor = node;
    // `opener` 链是有界的：正常嵌套每层都把游标移到浮层打开时的焦点节点（更浅的位置）。
    // 上限只是防御损坏的环（浮层的 opener 被设成它自己的后代），不会影响合法用例。
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

    /// R02-12（P1 回归）：**模态之上的弹出层属于模态的作用域**。
    ///
    /// 探针 1 的失败面：`create_overlay` 恒把浮层挂 root，旧判据只认
    /// `is_in_subtree(hit, scope)` ⇒ 对话框里的下拉/菜单必然落在作用域外，点击被整体吞掉。
    /// 正确语义（Qt 的模态子窗口/弹窗、Chrome 的 top layer、GTK 的 popover）：浮层的
    /// `opener` 在模态子树内（或 opener 又属于同一模态的另一个浮层）⇒ 该浮层仍在作用域内。
    #[test]
    fn a_popup_opened_from_inside_the_modal_stays_in_scope() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let modal = modal_overlay(&mut tree, root);
        let item = NodeBuilder::boxed().build(&mut tree, modal).expect("mount");
        let background = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        // 对话框里的下拉菜单：opener = 模态内的控件。
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
        // 打开者不在模态内的浮层仍然在作用域外。
        let stray = overlay_with_opener(
            &mut tree,
            root,
            OverlayDismiss::OutsideClick,
            Some(background),
        );
        let stray_item = NodeBuilder::boxed().build(&mut tree, stray).expect("mount");
        assert!(!in_modal_scope(&tree, stray_item, scope));
        // 没有 opener 的浮层同理。
        let orphan = overlay(&mut tree, root, OverlayDismiss::OutsideClick);
        let orphan_item = NodeBuilder::boxed()
            .build(&mut tree, orphan)
            .expect("mount");
        assert!(!in_modal_scope(&tree, orphan_item, scope));
    }

    /// 嵌套浮层递归：子菜单的 opener 是上一个菜单里的项，而上一个菜单属于模态。
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

    /// 损坏的 `opener` 环（浮层的 opener 指向自己的后代）不得让判定死循环。
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

//! a11y 树的侧表载荷与「节点 → `A11yNode`」的构建（`design/phase-11-a11y.md` §5 / §7.1）。
//!
//! 侧表（`NodeTree::a11y_slots`）与 `layout_cache` / `clip_keys` / `text_slots` / `debug_ids` 同型：
//! 按 arena 槽位索引、槽位复用时清空、`remove` 时逐槽清空。`Node` 因此保持 1144 B（F-114）。

use smallvec::SmallVec;

use crate::{
    a11y::{
        A11yDiagnostic, A11yLabel, A11yLabelSource, A11yNode, A11yNodeId, A11yOverride, A11yPatch,
        A11yRole,
        mapping::{
            bounds_of, effective_actions, effective_description, effective_label, effective_role,
            effective_value, is_disabled, is_hidden,
        },
    },
    math::Scale,
    node::{
        dependency::ReadCtx,
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
};

/// 一个节点的 a11y 载荷（侧表元素）。
///
/// `pub(crate)` 结构 + `pub` 字段（workspace lint 禁止字段级 `pub(crate)`；与 `NodeInteraction` 同款）。
pub(crate) struct NodeA11y {
    /// 静态声明（挂载时写入）。
    pub over: Option<A11yOverride>,
    /// 动态补丁闭包（`bind_a11y`）。
    pub bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>>,
    /// 上次求值的动态补丁（diff 用）。
    pub patch: A11yPatch,
}

impl NodeA11y {
    pub(crate) fn new(
        over: Option<A11yOverride>,
        bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>>,
    ) -> Self {
        Self {
            over,
            bind,
            patch: A11yPatch::default(),
        }
    }
}

impl std::fmt::Debug for NodeA11y {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeA11y")
            .field("over", &self.over)
            .field("bind", &self.bind.as_ref().map(|_| "<closure>"))
            .field("patch", &self.patch)
            .finish()
    }
}

/// 构建一个节点的 a11y 快照记录（`design §5.1` 的八步判定）。
///
/// 返回 `None` = 句柄陈旧（调用方已过滤，防御性）。诊断直接推进 `diagnostics`。
pub(crate) fn node_a11y(
    tree: &NodeTree,
    node: NodeId,
    scale: Scale,
    title: &str,
    diagnostics: &mut Vec<A11yDiagnostic>,
) -> Option<A11yNode> {
    let target = tree.get(node).ok()?;
    let slot = tree.a11y_of(node);
    let over = slot.and_then(|slot| slot.over.as_ref());
    let patch = slot.map(|slot| &slot.patch);

    let role = effective_role(tree, node, target, over, patch);
    let mut label = effective_label(target, role, over, patch);
    if node == tree.root() && label.is_none() && !title.is_empty() {
        label = Some(A11yLabel::new(
            smol_str::SmolStr::new(title),
            A11yLabelSource::Override,
        ));
    }
    let value = effective_value(target, role, over, patch);
    let description = effective_description(over, patch);
    // 与 `selected` / `expanded` 同型：布尔状态只走动态补丁（`A11yOverride` 不承载）。
    let checked = patch.and_then(|patch| patch.checked);
    let hidden = is_hidden(target, role, label.as_ref(), over, patch);
    let disabled = is_disabled(target, over, patch);
    let actions = effective_actions(tree, node, role, over, patch);

    // 诊断：自绘缺替代文本（§5.4）、交互节点无名（§5.2）。
    if role == A11yRole::Image
        && label.is_none()
        && let Some(kind) = match target.kind() {
            NodeKind::Canvas(_) => Some(crate::a11y::A11yDrawKind::Canvas),
            NodeKind::Geometry(_) => Some(crate::a11y::A11yDrawKind::Geometry),
            _ => None,
        }
    {
        diagnostics.push(A11yDiagnostic::MissingAltText { node, kind });
    }
    if label.is_none()
        && value.is_none()
        && matches!(
            role,
            A11yRole::Button
                | A11yRole::TextInput
                | A11yRole::MultilineTextInput
                | A11yRole::CheckBox
                | A11yRole::Switch
                | A11yRole::RadioButton
                | A11yRole::Slider
                | A11yRole::MenuItem
                | A11yRole::ListBoxOption
        )
    {
        diagnostics.push(A11yDiagnostic::MissingLabel { node });
    }

    let children = target
        .children()
        .iter()
        .map(|child| A11yNodeId::from_engine(*child))
        .collect();
    Some(A11yNode {
        id: A11yNodeId::from_engine(node),
        engine: node,
        role,
        label,
        value,
        description,
        bounds: bounds_of(tree, node, scale),
        actions,
        hidden,
        disabled,
        checked,
        selected: patch.and_then(|patch| patch.selected),
        expanded: patch.and_then(|patch| patch.expanded),
        children,
        parent: target.parent().map(A11yNodeId::from_engine),
    })
}

/// 把 `root` 及其整棵子树（DFS 先序）推进 `out`（去重由调用方决定）。
pub(crate) fn collect_subtree(tree: &NodeTree, root: NodeId, out: &mut SmallVec<[NodeId; 16]>) {
    let mut stack: SmallVec<[NodeId; 16]> = SmallVec::new();
    stack.push(root);
    while let Some(current) = stack.pop() {
        if tree.get(current).is_err() {
            continue;
        }
        out.push(current);
        if let Ok(target) = tree.get(current) {
            for child in target.children().iter().rev() {
                stack.push(*child);
            }
        }
    }
}

/// 从 `from` 的**父**开始向上，把祖先链推进 `out`（`from` 自身不加）。
pub(crate) fn push_ancestors(tree: &NodeTree, from: NodeId, out: &mut SmallVec<[NodeId; 16]>) {
    let mut cursor = tree.get(from).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        out.push(current);
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
}

/// 焦点回退：焦点节点必须**存在**于当前树里（否则 `accesskit_consumer` 的 `validate_global`
/// 会 panic），陈旧/已卸载时回退到窗口根。
///
/// 返回值第二项 = 需要记 `MissingFocusTarget` 诊断的原节点。
pub(crate) fn resolve_focus(tree: &NodeTree, focus: Option<NodeId>) -> (NodeId, Option<NodeId>) {
    match focus {
        Some(node) if tree.get(node).is_ok() => (node, None),
        Some(node) => (tree.root(), Some(node)),
        None => (tree.root(), None),
    }
}

#[cfg(test)]
mod tests {
    use smallvec::SmallVec;

    use super::{collect_subtree, push_ancestors, resolve_focus};
    use crate::node::{builder::NodeBuilder, id::NodeId, tree::NodeTree};

    fn chain(tree: &mut NodeTree) -> (NodeId, NodeId, NodeId) {
        let root = tree.root();
        let parent = NodeBuilder::boxed().build(tree, root).expect("parent");
        let child = NodeBuilder::boxed().build(tree, parent).expect("child");
        (root, parent, child)
    }

    #[test]
    fn collect_subtree_is_dfs_preorder() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        let sibling = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("sibling");
        let mut out = SmallVec::new();
        collect_subtree(&tree, root, &mut out);
        assert_eq!(out.as_slice(), &[root, parent, child, sibling]);
    }

    #[test]
    fn push_ancestors_walks_to_root() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        let mut out = SmallVec::new();
        push_ancestors(&tree, child, &mut out);
        assert_eq!(out.as_slice(), &[parent, root]);
    }

    #[test]
    fn resolve_focus_falls_back_to_root() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        assert_eq!(resolve_focus(&tree, None), (root, None));
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("node");
        assert_eq!(resolve_focus(&tree, Some(node)), (node, None));
    }

    #[test]
    fn resolve_focus_reports_a_removed_target() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let doomed = NodeBuilder::boxed().build(&mut tree, root).expect("node");
        tree.remove(doomed).expect("remove");
        let (focus, missing) = resolve_focus(&tree, Some(doomed));
        assert_eq!(focus, root, "陈旧焦点必须回退到根");
        assert_eq!(missing, Some(doomed), "并记一条 MissingFocusTarget");
    }
}

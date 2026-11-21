//! a11y 测试基建：`TreeUpdate` 影子树校验、机器可读快照、变异友好断言。
//!
//! 这一层回答的是「a11y 修复有没有被**能失败**的测试钉住」，而不是"引擎看起来对不对"：
//!
//! - [`ShadowTree`]：把一串 `accesskit::TreeUpdate` 按 AccessKit 的五条硬契约逐步应用
//!   （父必须已在树里或同批 / `children` 必须可达 / 每个节点只有一个父 / 被删子节点及其后代
//!   不得再出现 / `focus` 必须指向应用后的树里存在的节点）。R13-10 指出全仓没有一处
//!   `TreeUpdate` 端到端断言，而这五条恰好对应"脏位漏标、祖先漏发、父指针被双父覆盖、节点删了
//!   还在批里、焦点指向不存在的节点"五类 "测试全绿但 AT 崩" 的 bug。
//! - [`tree_update_snapshot`]：把 `TreeUpdate` 序列化成**确定性文本**（角色/名字/值/结构/
//!   焦点/动作/文本长度/排序/滚动/自定义动作），供快照测试审查语义变更。文本而非二进制：
//!   diff 可读、评审能看到"这一行 a11y 变了什么"。
//! - [`assert_contract_holds`] / [`assert_ids_are_unique`] / [`assert_node_covers`]：小而明确的
//!   断言 helper。它们的共同点是要么**有真实失败路径**（构造非法 update 必须报错），
//!   要么是双向断言（不写"检查了某个自建对象又读回来"的自证测试）。
//!
//! 这一层不依赖 `accesskit_consumer`（内部 crate），只依赖 `accesskit` 的公开类型。

use std::fmt::Write as _;

use accesskit::{NodeId as AkNodeId, TreeUpdate};

/// `TreeUpdate` 里一个节点的影子记录（只剩结构信息 + 一段可读摘要）。
#[derive(Clone, Debug, Default, PartialEq)]
struct ShadowNode {
    children: Vec<AkNodeId>,
    /// 快照摘要（`role:Button label=ok children=2`），用于全量/增量一致性比较。
    summary: String,
}

/// a11y 树影子：按顺序应用 `TreeUpdate` 并校验 AccessKit 的硬契约。
#[derive(Clone, Debug, Default)]
pub struct ShadowTree {
    nodes: ahash::HashMap<AkNodeId, ShadowNode>,
    focus: Option<AkNodeId>,
    /// 已应用的笔数（测试可断言"分了 N 批"）。
    updates: usize,
    /// 上一次全树笔的根（`TreeInfo.root`）。
    root: Option<AkNodeId>,
}

impl ShadowTree {
    /// 空影子。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 已应用的更新笔数。
    #[must_use]
    pub fn updates(&self) -> usize {
        self.updates
    }

    /// 树里的节点数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 当前焦点。
    #[must_use]
    pub fn focus(&self) -> Option<AkNodeId> {
        self.focus
    }

    /// 应用一笔 `TreeUpdate`。
    ///
    /// # Errors
    /// 违反 AccessKit 契约时返回**具体**的错误描述（哪个 id、哪条规则），供测试断言失败面。
    pub fn apply(&mut self, update: &TreeUpdate) -> Result<(), String> {
        if let Some(tree) = &update.tree {
            // 全树笔：消费端会整体重置树。
            self.nodes.clear();
            self.root = Some(tree.root);
            self.nodes.insert(
                tree.root,
                ShadowNode {
                    children: Vec::new(),
                    summary: String::new(),
                },
            );
        }
        let Some(root) = update
            .tree
            .as_ref()
            .map_or(self.root, |tree| Some(tree.root))
        else {
            return Err(String::from(
                "untracked tree: the first update must carry TreeInfo",
            ));
        };

        // ① 同一笔内每个节点：父必须已在树里或同批。
        let batch: ahash::HashSet<AkNodeId> = update.nodes.iter().map(|(id, _)| *id).collect();
        for (id, node) in &update.nodes {
            if *id == root {
                continue;
            }
            let parent_is_known = self
                .nodes
                .iter()
                .any(|(_, existing)| existing.children.contains(id))
                || update
                    .nodes
                    .iter()
                    .any(|(_, other)| other.children().contains(id));
            if !parent_is_known {
                return Err(format!(
                    "node {id:?} has no parent: it is neither the root, nor a child of an \
                     existing node, nor listed in another node's children in this batch"
                ));
            }
            let _ = node;
        }

        // ② `children` 里的 id 必须可达（已存在 / 同批）。
        for (id, node) in &update.nodes {
            for child in node.children() {
                let known = self.nodes.contains_key(child) || batch.contains(child);
                if !known {
                    return Err(format!(
                        "node {id:?} lists {child:?} in children, but that node is neither in \
                         the tree nor in this batch"
                    ));
                }
            }
        }

        // ②b 父唯一性：AccessKit 的树要求每个节点只有一个父。同一 id 出现在两个父的 `children`
        // 里时，consumer 的 parent 指针会被后处理者覆盖（结构与阅读顺序错乱）；旧的"有某个父"
        // 断言放过了这种双父 bug（E9b F1），这里作为硬契约钉死。
        let mut parents: ahash::HashMap<AkNodeId, AkNodeId> = ahash::HashMap::default();
        let kept = self
            .nodes
            .iter()
            .filter(|(id, _)| !batch.contains(id))
            .map(|(id, node)| (*id, node.children.as_slice()));
        let updated = update.nodes.iter().map(|(id, node)| (*id, node.children()));
        for (parent, children) in kept.chain(updated) {
            for child in children {
                if let Some(previous) = parents.insert(*child, parent)
                    && previous != parent
                {
                    return Err(format!(
                        "node {child:?} has multiple parents: {previous:?} and {parent:?}"
                    ));
                }
            }
        }

        // ③ 被删子节点及其后代不得在本批出现：父的旧 children 里消失的 id 必须在树里被移除。
        for (id, node) in &update.nodes {
            let Some(previous) = self.nodes.get(id) else {
                continue;
            };
            let next: ahash::HashSet<AkNodeId> = node.children().iter().copied().collect();
            for removed in previous
                .children
                .iter()
                .filter(|child| !next.contains(child))
            {
                let removed_tree = collect_shadow_subtree(&self.nodes, *removed);
                for gone in removed_tree {
                    if update.nodes.iter().any(|(candidate, _)| *candidate == gone) {
                        return Err(format!(
                            "node {gone:?} was removed from {id:?}'s children but is still \
                             present in this batch"
                        ));
                    }
                }
            }
        }

        // ④ 应用：先记下旧的 children（删子树要用旧列表），再写入/更新节点，
        //    最后把从父 children 里移除的节点整棵子树删掉。
        let previous: Vec<(AkNodeId, Vec<AkNodeId>)> = update
            .nodes
            .iter()
            .filter_map(|(id, _)| self.nodes.get(id).map(|node| (*id, node.children.clone())))
            .collect();
        for (id, node) in &update.nodes {
            let summary = node_summary(*id, node);
            self.nodes.insert(
                *id,
                ShadowNode {
                    children: node.children().to_vec(),
                    summary,
                },
            );
        }
        for (id, old_children) in previous {
            let Some(updated) = self.nodes.get(&id) else {
                continue;
            };
            let next: ahash::HashSet<AkNodeId> = updated.children.iter().copied().collect();
            for removed in old_children.iter().filter(|child| !next.contains(child)) {
                remove_shadow_subtree(&mut self.nodes, *removed);
            }
        }

        // ⑤ 焦点必须存在于应用后的树里。
        if !self.nodes.contains_key(&update.focus) {
            return Err(format!(
                "focus {:?} does not exist in the resulting tree",
                update.focus
            ));
        }
        self.focus = Some(update.focus);
        self.updates += 1;
        Ok(())
    }

    /// 节点摘要（`id -> summary`；全量 / 增量一致性比较用）。
    #[must_use]
    pub fn summaries(&self) -> ahash::HashMap<AkNodeId, String> {
        self.nodes
            .iter()
            .map(|(id, node)| (*id, node.summary.clone()))
            .collect()
    }

    /// 退出树里每个节点的 children 列表摘要（结构一致性用）。
    #[must_use]
    pub fn structure(&self) -> Vec<(AkNodeId, Vec<AkNodeId>)> {
        let mut rows: Vec<(AkNodeId, Vec<AkNodeId>)> = self
            .nodes
            .iter()
            .map(|(id, node)| (*id, node.children.clone()))
            .collect();
        rows.sort_by_key(|(id, _)| id.0);
        rows
    }
}

fn collect_shadow_subtree(
    nodes: &ahash::HashMap<AkNodeId, ShadowNode>,
    root: AkNodeId,
) -> Vec<AkNodeId> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        out.push(current);
        if let Some(node) = nodes.get(&current) {
            stack.extend(node.children.iter().copied());
        }
    }
    out
}

fn remove_shadow_subtree(nodes: &mut ahash::HashMap<AkNodeId, ShadowNode>, root: AkNodeId) {
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        if let Some(node) = nodes.remove(&current) {
            stack.extend(node.children);
        }
    }
}

/// 快照里值得审查的动作集合（`accesskit::Node` 没有公开的动作迭代器，只有 `supports_action`）。
const INTERESTING_ACTIONS: [accesskit::Action; 16] = [
    accesskit::Action::Click,
    accesskit::Action::Focus,
    accesskit::Action::Blur,
    accesskit::Action::SetValue,
    accesskit::Action::Increment,
    accesskit::Action::Decrement,
    accesskit::Action::SetTextSelection,
    accesskit::Action::ScrollUp,
    accesskit::Action::ScrollDown,
    accesskit::Action::ScrollLeft,
    accesskit::Action::ScrollRight,
    accesskit::Action::ScrollIntoView,
    accesskit::Action::SetScrollOffset,
    accesskit::Action::Expand,
    accesskit::Action::Collapse,
    accesskit::Action::CustomAction,
];

fn has_any_action(node: &accesskit::Node) -> bool {
    INTERESTING_ACTIONS
        .iter()
        .any(|action| node.supports_action(*action))
}

/// 单个节点的确定性文本摘要（角色 / 名字 / 值 / 结构 / 状态）。
fn node_summary(id: AkNodeId, node: &accesskit::Node) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{:?} role={:?} label={:?} value={:?}",
        id,
        node.role(),
        node.label(),
        node.value()
    );
    if !node.children().is_empty() {
        let _ = write!(out, " children={:?}", node.children());
    }
    if node.is_hidden() {
        let _ = write!(out, " hidden");
    }
    if node.is_disabled() {
        let _ = write!(out, " disabled");
    }
    if let Some(toggled) = node.toggled() {
        let _ = write!(out, " toggled={toggled:?}");
    }
    if let Some(selected) = node.is_selected() {
        let _ = write!(out, " selected={selected}");
    }
    if let Some(expanded) = node.is_expanded() {
        let _ = write!(out, " expanded={expanded}");
    }
    if has_any_action(node) {
        let mut actions: Vec<String> = INTERESTING_ACTIONS
            .iter()
            .copied()
            .filter(|action| node.supports_action(*action))
            .map(|action| format!("{action:?}"))
            .collect();
        actions.sort();
        let _ = write!(out, " actions={actions:?}");
    }
    if let Some(row) = node.row_index() {
        let _ = write!(out, " row={row}");
    }
    if let Some(column) = node.column_index() {
        let _ = write!(out, " column={column}");
    }
    if let Some(count) = node.row_count() {
        let _ = write!(out, " rows={count}");
    }
    if let Some(count) = node.column_count() {
        let _ = write!(out, " columns={count}");
    }
    if let Some(sort) = node.sort_direction() {
        let _ = write!(out, " sort={sort:?}");
    }
    if !node.character_lengths().is_empty() {
        let _ = write!(out, " character_lengths={:?}", node.character_lengths());
    }
    if !node.word_starts().is_empty() {
        let _ = write!(out, " word_starts={:?}", node.word_starts());
    }
    // 字符度量 / 文本方向（E4b）：caret 高亮能力是否发出、是否单行 LTR，直接进 oracle 摘要。
    if let Some(positions) = node.character_positions() {
        let _ = write!(out, " character_positions={positions:?}");
    }
    if let Some(widths) = node.character_widths() {
        let _ = write!(out, " character_widths={widths:?}");
    }
    if let Some(direction) = node.text_direction() {
        let _ = write!(out, " text_direction={direction:?}");
    }
    if let Some(selection) = node.text_selection() {
        let _ = write!(
            out,
            " selection={:?}..{:?}",
            selection.anchor.character_index, selection.focus.character_index
        );
    }
    if let Some(scroll) = node.scroll_x() {
        let _ = write!(out, " scroll_x={scroll}");
    }
    if let Some(scroll) = node.scroll_y() {
        let _ = write!(out, " scroll_y={scroll}");
    }
    if node.clips_children() {
        let _ = write!(out, " clips_children");
    }
    if !node.custom_actions().is_empty() {
        let mut custom: Vec<String> = node
            .custom_actions()
            .iter()
            .map(|action| format!("{}:{}", action.id, action.description))
            .collect();
        custom.sort();
        let _ = write!(out, " custom_actions={custom:?}");
    }
    out
}

/// `TreeUpdate` 的确定性机器可读快照（**文本**，供快照测试 / 评审 diff）。
///
/// 格式：每行一个节点，按 id 升序；最后一行是焦点与树信息。字段顺序固定，空的属性不输出。
#[must_use]
pub fn tree_update_snapshot(update: &TreeUpdate) -> String {
    let mut rows: Vec<(u64, String)> = update
        .nodes
        .iter()
        .map(|(id, node)| (id.0, node_summary(*id, node)))
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "tree: {} ({} node(s))",
        update
            .tree
            .as_ref()
            .map_or(String::from("incremental"), |tree| format!(
                "root={:?} toolkit={}/{}",
                tree.root,
                tree.toolkit_name.as_deref().unwrap_or("?"),
                tree.toolkit_version.as_deref().unwrap_or("?")
            )),
        update.nodes.len()
    );
    for (_, row) in rows {
        let _ = writeln!(out, "{row}");
    }
    let _ = writeln!(out, "focus: {:?}", update.focus);
    out
}

/// 断言一笔 update 满足 AccessKit 契约（`shadow` 必须已经被之前的笔推进到同一状态）。
///
/// # Panics
/// 契约失败（错误信息带"哪个 id / 哪条规则"）。
pub fn assert_contract_holds(shadow: &mut ShadowTree, update: &TreeUpdate) {
    if let Err(error) = shadow.apply(update) {
        panic!(
            "TreeUpdate violates the AccessKit contract: {error}\n--- update ---\n{}",
            tree_update_snapshot(update)
        );
    }
}

/// 断言一批 id 互不相同（身份分配器的"不同节点不冲突"）。
///
/// # Panics
/// 出现重复 id（打印重复项）。
pub fn assert_ids_are_unique(ids: impl IntoIterator<Item = AkNodeId>) {
    let mut seen: ahash::HashSet<AkNodeId> = ahash::HashSet::default();
    let mut duplicates = Vec::new();
    for id in ids {
        if !seen.insert(id) {
            duplicates.push(id);
        }
    }
    assert!(
        duplicates.is_empty(),
        "duplicate accessible node id(s): {duplicates:?}"
    );
}

/// 断言"全部改变"的节点集合确实是增量（不是变相全量）。
///
/// 这是给"预算/增量"测试用的变异友好断言：把增量退回全量、或把预算调成无限，
/// `nodes.len() <= max` 立刻红。
///
/// # Panics
/// 本笔节点数超过 `max`。
pub fn assert_incremental_within(update: &TreeUpdate, max: usize) {
    assert!(
        update.nodes.len() <= max,
        "expected an incremental update with <= {max} node(s), got {}:\n{}",
        update.nodes.len(),
        tree_update_snapshot(update)
    );
}

#[cfg(test)]
mod tests {
    use accesskit::{Node, NodeId as AkNodeId, Role, TreeId, TreeInfo, TreeUpdate};

    use super::{
        ShadowTree, assert_ids_are_unique, assert_incremental_within, tree_update_snapshot,
    };

    fn node(role: Role, children: &[AkNodeId]) -> Node {
        let mut node = Node::new(role);
        if !children.is_empty() {
            node.set_children(children.to_vec());
        }
        node
    }

    fn full(nodes: Vec<(AkNodeId, Node)>, focus: AkNodeId) -> TreeUpdate {
        TreeUpdate {
            nodes,
            tree: Some(TreeInfo {
                root: AkNodeId(1),
                toolkit_name: Some(String::from("Kytrix")),
                toolkit_version: Some(String::from("0.0.3")),
            }),
            tree_id: TreeId::ROOT,
            focus,
        }
    }

    fn incremental(nodes: Vec<(AkNodeId, Node)>, focus: AkNodeId) -> TreeUpdate {
        TreeUpdate {
            nodes,
            tree: None,
            tree_id: TreeId::ROOT,
            focus,
        }
    }

    /// 合法的一串更新必须全部通过（含删节点时父重发）。
    #[test]
    fn a_valid_update_sequence_is_accepted() {
        let mut shadow = ShadowTree::new();
        // 首笔：root → [button]
        shadow
            .apply(&full(
                vec![
                    (AkNodeId(1), node(Role::Window, &[AkNodeId(2)])),
                    (AkNodeId(2), node(Role::Button, &[])),
                ],
                AkNodeId(1),
            ))
            .expect("first update");
        assert_eq!(shadow.len(), 2);
        // 增量：加一个子节点，父重发。
        shadow
            .apply(&incremental(
                vec![
                    (AkNodeId(1), node(Role::Window, &[AkNodeId(2), AkNodeId(3)])),
                    (AkNodeId(3), node(Role::Label, &[])),
                ],
                AkNodeId(3),
            ))
            .expect("structure update");
        assert_eq!(shadow.len(), 3);
        assert_eq!(shadow.focus(), Some(AkNodeId(3)));
        assert_eq!(shadow.updates(), 2);
    }

    /// 契约 ①：没有父的孤儿节点必须被拒绝。
    #[test]
    fn an_orphan_node_is_rejected() {
        let mut shadow = ShadowTree::new();
        shadow
            .apply(&full(
                vec![(AkNodeId(1), node(Role::Window, &[]))],
                AkNodeId(1),
            ))
            .expect("first");
        let update = incremental(vec![(AkNodeId(9), node(Role::Button, &[]))], AkNodeId(1));
        let error = shadow.apply(&update).expect_err("孤儿必须被拒绝");
        assert!(error.contains("no parent"), "{error}");
    }

    /// 契约 ②b：同一节点不得同时挂在两个父下（"双父"会让 consumer 的 parent 指针被覆盖）。
    #[test]
    fn a_node_with_two_parents_is_rejected() {
        let mut shadow = ShadowTree::new();
        shadow
            .apply(&full(
                vec![
                    (AkNodeId(1), node(Role::Window, &[AkNodeId(2)])),
                    (AkNodeId(2), node(Role::Button, &[])),
                ],
                AkNodeId(1),
            ))
            .expect("first");
        // 同一笔里 3 既挂在 root 下、又挂在 2 下 ⇒ 双父，非法。
        let update = incremental(
            vec![
                (AkNodeId(1), node(Role::Window, &[AkNodeId(2), AkNodeId(3)])),
                (AkNodeId(2), node(Role::Button, &[AkNodeId(3)])),
                (AkNodeId(3), node(Role::Label, &[])),
            ],
            AkNodeId(1),
        );
        let error = shadow.apply(&update).expect_err("双父必须被拒绝");
        assert!(error.contains("multiple parents"), "{error}");
    }

    /// 契约 ④：被删子节点仍在本批出现必须被拒绝（"节点删了还在批里"）。
    #[test]
    fn a_removed_child_must_not_reappear_in_the_batch() {
        let mut shadow = ShadowTree::new();
        shadow
            .apply(&full(
                vec![
                    (AkNodeId(1), node(Role::Window, &[AkNodeId(2)])),
                    (AkNodeId(2), node(Role::Button, &[])),
                ],
                AkNodeId(1),
            ))
            .expect("first");
        // 父不再列出 2，但同一笔里 2 仍然被更新 ⇒ 非法。
        let update = incremental(
            vec![
                (AkNodeId(1), node(Role::Window, &[])),
                (AkNodeId(2), node(Role::Button, &[])),
            ],
            AkNodeId(1),
        );
        let error = shadow.apply(&update).expect_err("被删节点不得再出现");
        assert!(error.contains("still present"), "{error}");
    }

    /// 契约 ⑤：焦点必须指向结果树里存在的节点（"焦点指向未发出的节点"）。
    #[test]
    fn a_focus_outside_the_tree_is_rejected() {
        let mut shadow = ShadowTree::new();
        shadow
            .apply(&full(
                vec![(AkNodeId(1), node(Role::Window, &[]))],
                AkNodeId(1),
            ))
            .expect("first");
        let update = incremental(vec![], AkNodeId(42));
        let error = shadow.apply(&update).expect_err("不存在的焦点必须被拒绝");
        assert!(error.contains("focus"), "{error}");
    }

    /// 快照是确定性的：同样的 update 输出同样文本；节点按 id 升序。
    #[test]
    fn the_snapshot_is_deterministic_and_sorted() {
        let update = full(
            vec![
                (AkNodeId(2), node(Role::Button, &[])),
                (AkNodeId(1), node(Role::Window, &[AkNodeId(2)])),
            ],
            AkNodeId(1),
        );
        let first = tree_update_snapshot(&update);
        let second = tree_update_snapshot(&update);
        assert_eq!(first, second);
        let lines: Vec<&str> = first.lines().collect();
        assert!(lines[0].starts_with("tree: root="), "{}", lines[0]);
        assert!(lines[1].contains("role=Window"), "{}", lines[1]);
        assert!(lines[2].contains("role=Button"), "{}", lines[2]);
        assert!(first.contains("focus: #1"), "{first}");
    }

    /// 断言 helper 必须**能失败**（否则它们就是又一个假绿）。
    #[test]
    fn the_assertion_helpers_detect_violations() {
        let update = full(
            vec![
                (AkNodeId(1), node(Role::Window, &[])),
                (AkNodeId(1), node(Role::Button, &[])),
                (AkNodeId(1), node(Role::Label, &[])),
            ],
            AkNodeId(1),
        );
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_ids_are_unique(update.nodes.iter().map(|(id, _)| *id));
        }));
        assert!(outcome.is_err(), "重复 id 必须让断言失败");

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_incremental_within(&update, 2);
        }));
        assert!(outcome.is_err(), "超预算必须让断言失败");
    }

    /// R13-13：滚动容器必须把 `scroll_*` 状态暴露到 `TreeUpdate`（UIA 的 ScrollPattern /
    /// AT-SPI 的 Value 读这几项；旧实现宣告了滚动动作却没有任何滚动几何）。
    #[test]
    fn scroll_state_reaches_the_tree_update() {
        use crate::{
            a11y::A11yOverride,
            interaction::ScrollConfig,
            math::Rect,
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
            test_support::app::TestApp,
        };

        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let container = NodeBuilder::boxed()
            .style(Style {
                clip: Clip::Bounds,
                ..Style::default()
            })
            .layout_override(Rect::new(0.0, 0.0, 100.0, 80.0))
            .a11y(A11yOverride {
                label: Some(smol_str::SmolStr::new_static("盘口")),
                ..A11yOverride::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("container");
        let _ = app.drain().expect("首帧");
        let handle = app
            .app_mut()
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.app_mut()
            .set_scroll_content(&handle, crate::math::Size::new(100.0, 800.0))
            .expect("content");
        app.app_mut()
            .scroll_to(&handle, crate::math::Point::new(0.0, 120.0))
            .expect("scroll to");
        let _ = app.drain().expect("滚动帧");

        let snapshot = tree_update_snapshot(app.a11y_update().expect("滚动笔"));
        assert!(
            snapshot.contains("scroll_y=120"),
            "滚动偏移必须进 TreeUpdate：\n{snapshot}"
        );
        assert!(snapshot.contains("clips_children"), "{snapshot}");
    }
}

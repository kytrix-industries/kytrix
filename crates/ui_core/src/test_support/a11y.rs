use std::fmt::Write as _;

use accesskit::{NodeId as AkNodeId, TreeUpdate};

#[derive(Clone, Debug, Default, PartialEq)]
struct ShadowNode {
    children: Vec<AkNodeId>,
    summary: String,
}

#[derive(Clone, Debug, Default)]
pub struct ShadowTree {
    nodes: ahash::HashMap<AkNodeId, ShadowNode>,
    focus: Option<AkNodeId>,
    updates: usize,
    root: Option<AkNodeId>,
}

impl ShadowTree {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn updates(&self) -> usize {
        self.updates
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[must_use]
    pub fn focus(&self) -> Option<AkNodeId> {
        self.focus
    }

    pub fn apply(&mut self, update: &TreeUpdate) -> Result<(), String> {
        if let Some(tree) = &update.tree {
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

    #[must_use]
    pub fn summaries(&self) -> ahash::HashMap<AkNodeId, String> {
        self.nodes
            .iter()
            .map(|(id, node)| (*id, node.summary.clone()))
            .collect()
    }

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
    if let Some(orientation) = node.orientation() {
        let _ = write!(out, " orientation={orientation:?}");
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

pub fn assert_contract_holds(shadow: &mut ShadowTree, update: &TreeUpdate) {
    if let Err(error) = shadow.apply(update) {
        panic!(
            "TreeUpdate violates the AccessKit contract: {error}\n--- update ---\n{}",
            tree_update_snapshot(update)
        );
    }
}

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

    #[test]
    fn a_valid_update_sequence_is_accepted() {
        let mut shadow = ShadowTree::new();
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

    #[test]
    fn scroll_state_reaches_the_tree_update() {
        use crate::{
            a11y::A11yOverride,
            interaction::ScrollConfig,
            math::{LogicalRect, px},
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
            test_support::app::TestApp,
        };

        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let mut container_style = Style::default();
        container_style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(container_style)
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(80.0)))
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
            .set_scroll_content(&handle, crate::math::LogicalSize::new(px(100.0), px(800.0)))
            .expect("content");
        app.app_mut()
            .scroll_to(&handle, crate::math::LogicalPoint::new(px(0.0), px(120.0)))
            .expect("scroll to");
        let _ = app.drain().expect("滚动帧");

        let snapshot = tree_update_snapshot(app.a11y_update().expect("滚动笔"));
        assert!(
            snapshot.contains("scroll_y=120"),
            "滚动偏移必须进 TreeUpdate：\n{snapshot}"
        );
        assert!(snapshot.contains("clips_children"), "{snapshot}");
    }

    #[test]
    fn a_modal_scope_change_is_delivered_as_a_legal_incremental_update() {
        use crate::{
            a11y::{A11yOverride, A11yRole},
            interaction::{OverlayDismiss, OverlayOptions, OverlayPlacement},
            math::{LogicalPoint, LogicalRect, px},
            node::builder::NodeBuilder,
            test_support::app::TestApp,
        };

        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let background = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(40.0)))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("下单")),
                ..A11yOverride::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("background");
        let _ = app.drain().expect("首帧");
        app.assert_a11y_contract();
        assert!(
            app.a11y_update().expect("首笔 a11y 交付").tree.is_some(),
            "首笔必须带 TreeInfo"
        );

        let modal = app
            .app_mut()
            .create_overlay(
                window,
                OverlayOptions {
                    placement: OverlayPlacement::Point(LogicalPoint::new(px(150.0), px(50.0))),
                    dismiss: OverlayDismiss::Explicit,
                    trap_focus: true,
                    a11y: A11yOverride {
                        role: Some(A11yRole::Dialog),
                        label: Some(smol_str::SmolStr::new_static("确认下单")),
                        ..A11yOverride::default()
                    },
                    ..OverlayOptions::default()
                },
            )
            .expect("modal");
        NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(60.0), px(24.0)))
            .on_click(crate::event::Action::custom("modal.ok"))
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                modal.node(),
            )
            .expect("confirm");
        let _ = app.drain().expect("模态帧");
        app.assert_a11y_contract();

        let update = app.a11y_update().expect("模态打开必须产生 a11y 增量");
        let snapshot = app.a11y().expect("本帧快照");
        let background_id =
            accesskit::NodeId(snapshot.node_for(background).expect("背景记录").id.get());
        let background_node = update
            .nodes
            .iter()
            .find(|(id, _)| *id == background_id)
            .map(|(_, node)| node)
            .expect("背景的 hidden 变化必须在本笔增量里（模态作用域变化要标全树 A11Y）");
        assert!(background_node.is_hidden(), "模态外背景必须 hidden");
        let wrapper = snapshot.node_for(modal.node()).expect("模态包装记录");
        assert_eq!(wrapper.role, A11yRole::Dialog);
        assert!(wrapper.modal, "a11y modal 必须跟 trap_focus 走");
        let wrapper_id = accesskit::NodeId(wrapper.id.get());
        let wrapper_ak = update
            .nodes
            .iter()
            .find(|(id, _)| *id == wrapper_id)
            .map(|(_, node)| node)
            .expect("模态包装必须在本笔增量里");
        assert!(wrapper_ak.is_modal());

        app.app_mut().close_overlay(&modal);
        let _ = app.drain().expect("关闭帧");
        app.assert_a11y_contract();
        let update = app.a11y_update().expect("模态关闭必须产生 a11y 增量");
        let snapshot = app.a11y().expect("关闭帧快照");
        let background_id =
            accesskit::NodeId(snapshot.node_for(background).expect("背景记录").id.get());
        let background_node = update
            .nodes
            .iter()
            .find(|(id, _)| *id == background_id)
            .map(|(_, node)| node)
            .expect("背景恢复可见必须在增量里（对称标全树 A11Y）");
        assert!(!background_node.is_hidden(), "模态关闭后背景必须恢复可见");
    }
}

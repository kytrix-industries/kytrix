//! `A11yBridge`：变更集合、快照产出与 `accesskit::TreeUpdate` 组装（`design/phase-11-a11y.md` §6 / §7）。
//!
//! 两条纪律：
//! 1. **懒激活**：`TreeUpdate` 只在 `accesskit_windows` 的 `update_if_active` 闭包里构造
//!    （adapter 不活跃时闭包不执行 ⇒ 零 accesskit 开销）。引擎侧的 [`A11yBridge::take`] 只做
//!    「收集 id + 清位 + 建快照」，不构造任何 `accesskit::Node`。
//! 2. **单一 drainer**：`take` 会消费 `A11Y|ORDER` 位与 `full_pending`，同一时刻只能有一个消费者
//!    （生产 = 平台层；`test-support` 的捕获路径只在 `TestApp` 里打开）。

use accesskit::{NodeId as AkNodeId, Rect as AkRect, TreeId, TreeInfo, TreeUpdate};
use smallvec::SmallVec;

use crate::{
    a11y::{
        A11yDiagnostic, A11yLabelSource, A11yRole, A11yTreeSnapshot,
        mapping::{action_to_accesskit, role_to_accesskit},
        tree::{collect_subtree, node_a11y, push_ancestors, resolve_focus},
    },
    app::App,
    math::Scale,
    node::{dirty::DirtyFlags, id::NodeId, tree::NodeTree},
    window::WindowId,
};

/// 一个窗口的 a11y 状态机（`Window.a11y`）。
pub(crate) struct A11yBridge {
    /// 下一笔必须是全树（首次交付 / scale 变化 / 捕获开启）。
    full_pending: bool,
    /// 布局比例代际（`Window::set_scale` 递增；变化 ⇒ 全树重算 bounds）。
    bounds_epoch: u32,
    /// 上一笔交付时的代际。
    seen_bounds_epoch: u32,
    /// 内容整体平移的子树（滚动容器）：子孙 bounds 全变。
    subtrees: SmallVec<[NodeId; 2]>,
    /// 有 `bind_a11y` 节点被写信号（没有像素要画，但 `refresh_styles` 必须跑一次）。
    bind_work: bool,
    /// 测试捕获开关（生产构建里整个字段不存在：捕获只有 `test-support` 的 `TestApp` 会用）。
    #[cfg(feature = "test-support")]
    capture: bool,
}

impl A11yBridge {
    /// 新建（首笔必为全树）。
    pub(crate) fn new() -> Self {
        Self {
            full_pending: true,
            bounds_epoch: 0,
            seen_bounds_epoch: 0,
            subtrees: SmallVec::new(),
            bind_work: false,
            #[cfg(feature = "test-support")]
            capture: false,
        }
    }

    /// 本轮是否有 a11y 工作（O(1)：脏位计数 + 两个旗标）。
    pub(crate) fn pending(&self, tree: &NodeTree) -> bool {
        self.full_pending
            || self.bounds_epoch != self.seen_bounds_epoch
            || !self.subtrees.is_empty()
            || tree.a11y_dirty() > 0
    }

    /// DPI 比例变化（bounds 全变 ⇒ 下一笔全树）。
    pub(crate) fn note_bounds_changed(&mut self) {
        self.bounds_epoch = self.bounds_epoch.wrapping_add(1);
    }

    /// 只有 `bind_a11y` 的节点被写信号：记下来让下一帧仍跑 `refresh_styles`（不产生像素）。
    pub(crate) fn note_bind_work(&mut self) {
        self.bind_work = true;
    }

    /// 是否有待求值的 a11y 绑定（`refresh_styles` 开头清掉）。
    pub(crate) fn bind_work(&self) -> bool {
        self.bind_work
    }

    /// 清掉「a11y 绑定待求值」旗标（`refresh_styles` 的入口处）。
    pub(crate) fn clear_bind_work(&mut self) {
        self.bind_work = false;
    }

    /// 某个子树的内容整体平移（滚动）：下一笔重发整棵子树。
    pub(crate) fn invalidate_subtree(&mut self, node: NodeId) {
        if !self.subtrees.contains(&node) {
            self.subtrees.push(node);
        }
    }

    /// 打开/关闭测试捕获。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_capture(&mut self, on: bool) {
        if on && !self.capture {
            self.full_pending = true;
        }
        self.capture = on;
    }

    /// 产出一笔交付（同时消费 `A11Y|ORDER` 位与 `subtrees`）。`None` = 没有工作。
    pub(crate) fn take(
        &mut self,
        tree: &mut NodeTree,
        focus: Option<NodeId>,
        scale: Scale,
        title: &str,
        diagnostics: &mut Vec<A11yDiagnostic>,
    ) -> Option<A11yTreeSnapshot> {
        let full = self.full_pending || self.bounds_epoch != self.seen_bounds_epoch;
        if !full && tree.a11y_dirty() == 0 && self.subtrees.is_empty() {
            return None;
        }

        // ① 候选集合：全树 = 全部节点；增量 = 脏节点 + 滚动子树 + 每个候选的祖先链。
        let mut candidates: SmallVec<[NodeId; 16]> = SmallVec::new();
        if full {
            candidates.extend(tree.order().iter().copied());
        } else {
            for node in tree.order() {
                if tree.get(*node).is_ok_and(|target| {
                    target
                        .dirty()
                        .intersects(DirtyFlags::A11Y | DirtyFlags::ORDER)
                }) {
                    candidates.push(*node);
                }
            }
            for root in std::mem::take(&mut self.subtrees) {
                collect_subtree(tree, root, &mut candidates);
            }
            // 祖先链：结构增删必须同批发父（`accesskit_consumer` 的硬约束），
            // 无条件带上（未变化的节点在消费端不会产生 updated 事件，只付带宽）。
            let mut ancestors: SmallVec<[NodeId; 16]> = SmallVec::new();
            for node in candidates.iter() {
                push_ancestors(tree, *node, &mut ancestors);
            }
            for ancestor in ancestors {
                if !candidates.contains(&ancestor) {
                    candidates.push(ancestor);
                }
            }
        }

        // ② 清位（全树路径同样清，避免位永久累积）。
        for node in candidates.iter() {
            tree.clear_a11y(*node);
        }

        // ③ 建节点：按 `order()` 序（快照的确定性来自这里）。
        let mut nodes = Vec::new();
        if full {
            for node in tree.order() {
                if let Some(record) = node_a11y(tree, *node, scale, title, diagnostics) {
                    nodes.push(record);
                }
            }
        } else {
            for node in tree.order() {
                if candidates.contains(node)
                    && let Some(record) = node_a11y(tree, *node, scale, title, diagnostics)
                {
                    nodes.push(record);
                }
            }
        }

        // ④ 焦点（必带；陈旧 ⇒ 回退到根 + 诊断）。
        let (focus_id, missing) = resolve_focus(tree, focus);
        if let Some(missing) = missing {
            diagnostics.push(A11yDiagnostic::MissingFocusTarget { node: missing });
        }
        // 焦点节点的记录是**独立**的一份：增量笔里它通常不在 `nodes` 里。
        // 诊断用临时缓冲，避免同一个节点被记两遍。
        let mut scratch: Vec<A11yDiagnostic> = Vec::new();
        let focus_node = node_a11y(tree, focus_id, scale, title, &mut scratch)
            .expect("resolve_focus 已保证焦点节点存活");

        self.full_pending = false;
        self.seen_bounds_epoch = self.bounds_epoch;
        Some(A11yTreeSnapshot {
            full,
            nodes,
            focus: crate::a11y::A11yNodeId::from_engine(focus_id),
            focus_node,
            diagnostics: diagnostics.len(),
        })
    }
}

impl Default for A11yBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// 生产/捕获的唯一物化入口：跑一笔 pass 并转成 `accesskit::TreeUpdate`。
///
/// **必须在 `update_if_active` 的闭包里调用**（adapter 不活跃时闭包不执行 ⇒ 零开销）。
pub(crate) fn build_update(app: &mut App, window: WindowId) -> TreeUpdate {
    let Some(root) = app.window(window).map(|target| target.tree().root()) else {
        // 窗口已关闭：调用方（平台泵）不会走到这里；给一个空更新而不是 panic。
        return TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: TreeId::ROOT,
            focus: AkNodeId(0),
        };
    };
    match app.a11y_take(window) {
        Some(snapshot) => to_tree_update(&snapshot, root),
        // 防御：`a11y_pending` 为真时 `take` 必产出；真走到这里说明调用方没判 pending。
        None => TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: TreeId::ROOT,
            focus: AkNodeId(crate::a11y::A11yNodeId::from_engine(root).get()),
        },
    }
}

/// 快照 → `accesskit::TreeUpdate`（`tree: Some(..)` 只在全树那一笔出现）。
pub(crate) fn to_tree_update(snapshot: &A11yTreeSnapshot, root: NodeId) -> TreeUpdate {
    let tree = snapshot.full.then(|| TreeInfo {
        root: AkNodeId(crate::a11y::A11yNodeId::from_engine(root).get()),
        toolkit_name: Some(String::from("Kytrix")),
        toolkit_version: Some(String::from(env!("CARGO_PKG_VERSION"))),
    });
    let mut nodes = Vec::with_capacity(snapshot.nodes.len());
    for record in &snapshot.nodes {
        nodes.push((AkNodeId(record.id.get()), to_accesskit(record)));
    }
    TreeUpdate {
        nodes,
        tree,
        tree_id: TreeId::ROOT,
        focus: AkNodeId(snapshot.focus.get()),
    }
}

/// 快照记录 → `accesskit::Node`（`design §4.3` / §5.3 的写入规则）。
pub(crate) fn to_accesskit(record: &crate::a11y::A11yNode) -> accesskit::Node {
    let mut node = accesskit::Node::new(role_to_accesskit(record.role));
    if !record.children.is_empty() {
        node.set_children(
            record
                .children
                .iter()
                .map(|child| AkNodeId(child.get()))
                .collect::<Vec<_>>(),
        );
    }
    if record.hidden {
        node.set_hidden();
    }
    if record.disabled {
        node.set_disabled();
    }
    // `Label` 角色的可访问名来自 `value`（`accesskit_consumer::label_comes_from_value`）。
    if let Some(label) = &record.label {
        if record.role == A11yRole::Label || label.source() == A11yLabelSource::TextContent {
            node.set_value(label.text());
        } else {
            node.set_label(label.text());
        }
    }
    if let Some(value) = &record.value {
        node.set_value(value.as_str());
    }
    if let Some(description) = &record.description {
        node.set_description(description.as_str());
    }
    if let Some(selected) = record.selected {
        node.set_selected(selected);
    }
    if let Some(expanded) = record.expanded {
        node.set_expanded(expanded);
    }
    for action in &record.actions {
        node.add_action(action_to_accesskit(*action));
    }
    let bounds = record.bounds;
    node.set_bounds(AkRect::new(
        f64::from(bounds.x0),
        f64::from(bounds.y0),
        f64::from(bounds.x1),
        f64::from(bounds.y1),
    ));
    node
}

#[cfg(test)]
mod tests {
    use accesskit::{Action, NodeId as AkNodeId};
    use smol_str::SmolStr;

    use super::{A11yBridge, to_accesskit, to_tree_update};
    use crate::{
        a11y::{A11yAction, A11yOverride, A11yPatch, A11yRole},
        math::Scale,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::{canvas::Canvas, style::StylePatch},
        text::TextStyle,
    };

    fn scale() -> Scale {
        Scale::ONE
    }

    fn take(
        bridge: &mut A11yBridge,
        tree: &mut NodeTree,
        focus: Option<NodeId>,
    ) -> Option<crate::a11y::A11yTreeSnapshot> {
        let mut diagnostics = Vec::new();
        bridge.take(tree, focus, scale(), "Kytrix", &mut diagnostics)
    }

    #[test]
    fn first_delivery_is_full_and_then_incremental() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed()
            .on_click(crate::event::Action::custom("ok"))
            .a11y(A11yOverride {
                label: Some(SmolStr::new_static("提交")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("mount");
        let mut bridge = A11yBridge::new();

        let full = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(full.full);
        assert_eq!(full.nodes.len(), tree.order().len());
        assert_eq!(full.focus, crate::a11y::A11yNodeId::from_engine(root));
        assert_eq!(
            full.node_for(button).expect("button").role,
            A11yRole::Button
        );
        assert!(!bridge.pending(&tree), "交付后必须无工作");

        assert!(
            take(&mut bridge, &mut tree, None).is_none(),
            "无变化时不得产出"
        );

        // 只改一个 label ⇒ 只含该节点 + 祖先。
        tree.set_a11y_patch(
            button,
            A11yPatch {
                label: Some(SmolStr::new_static("提交（改）")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(button);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(!patch.full);
        assert_eq!(
            patch
                .nodes
                .iter()
                .map(|node| node.engine)
                .collect::<Vec<_>>(),
            vec![root, button],
            "增量 = 变更节点 + 祖先链"
        );
        assert_eq!(
            patch.node_for(button).expect("button").label_text(),
            Some("提交（改）")
        );
    }

    /// 审查 #4：焦点节点不在本笔 `nodes` 里时，`focus_node()` 仍必须给出它（不 panic）。
    #[test]
    fn focus_node_is_carried_even_when_it_is_not_in_the_batch() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        // 只标根 ⇒ 本笔 nodes 只有根；焦点指向 button。
        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, Some(button)).expect("增量");
        assert_eq!(
            patch
                .nodes
                .iter()
                .map(|node| node.engine)
                .collect::<Vec<_>>(),
            vec![root],
            "本笔只含根"
        );
        assert!(
            patch
                .node(crate::a11y::A11yNodeId::from_engine(button))
                .is_none(),
            "焦点节点不在 nodes 里"
        );
        assert_eq!(patch.focus(), crate::a11y::A11yNodeId::from_engine(button));
        assert_eq!(
            patch.focus_node().engine,
            button,
            "焦点节点必须独立携带（不依赖 nodes）"
        );
    }

    #[test]
    fn structure_change_resends_the_parent_children() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = NodeBuilder::boxed().build(&mut tree, root).expect("parent");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        let child = NodeBuilder::boxed()
            .build(&mut tree, parent)
            .expect("child");
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let parent_node = patch.node_for(parent).expect("父必须重发");
        assert!(
            parent_node
                .children
                .contains(&crate::a11y::A11yNodeId::from_engine(child))
        );
        assert!(patch.node_for(child).is_some(), "新子节点必须出现");

        // 卸载：父的 children 缩短，被删节点**不出现**。
        tree.remove(child).expect("remove");
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(patch.node_for(child).is_none(), "被删节点不得出现");
        assert!(
            patch
                .node_for(parent)
                .expect("父必须重发")
                .children
                .is_empty()
        );
    }

    #[test]
    fn focus_is_always_carried_and_falls_back_to_root() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        tree.mark_a11y(button);
        let patch = take(&mut bridge, &mut tree, Some(button)).expect("焦点");
        assert_eq!(patch.focus, crate::a11y::A11yNodeId::from_engine(button));
        assert!(patch.node(patch.focus).is_some(), "焦点必须在快照里");

        tree.remove(button).expect("remove");
        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, Some(button)).expect("陈旧焦点");
        assert_eq!(
            patch.focus,
            crate::a11y::A11yNodeId::from_engine(root),
            "陈旧焦点回退到根"
        );
    }

    #[test]
    fn hidden_disabled_and_patch_fields_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("node");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        tree.set_a11y_patch(
            node,
            A11yPatch {
                label: Some(SmolStr::new_static("量")),
                value: Some(SmolStr::new_static("42")),
                disabled: Some(true),
                hidden: Some(true),
                selected: Some(true),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(node);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let record = patch.node_for(node).expect("node");
        assert!(record.hidden);
        assert!(record.disabled);
        assert_eq!(record.selected, Some(true));
        assert_eq!(record.label_text(), Some("量"));
        assert_eq!(record.value.as_deref(), Some("42"));

        let converted = to_accesskit(record);
        assert!(converted.is_hidden());
        assert!(converted.is_disabled());
        assert_eq!(converted.is_selected(), Some(true));
        assert_eq!(converted.label(), Some("量"));
        assert_eq!(converted.value(), Some("42"));
        let _ = StylePatch::default();
    }

    #[test]
    fn text_node_writes_value_not_label() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let text = NodeBuilder::text("价格", TextStyle::new(12.0))
            .build(&mut tree, root)
            .expect("text");
        let mut bridge = A11yBridge::new();
        let snapshot = take(&mut bridge, &mut tree, None).expect("首笔");
        let record = snapshot.node_for(text).expect("text");
        assert_eq!(record.role, A11yRole::Label);
        assert_eq!(
            record.label.as_ref().map(|l| l.source()),
            Some(crate::a11y::A11yLabelSource::TextContent)
        );
        let converted = to_accesskit(record);
        assert_eq!(converted.label(), None, "Label 的 label 不参与命名");
        assert_eq!(converted.value(), Some("价格"));
    }

    #[test]
    fn canvas_without_alt_text_is_hidden_with_a_diagnostic() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(crate::math::Rect::new(0.0, 0.0, 8.0, 8.0)))
            .build(&mut tree, root)
            .expect("canvas");
        let mut bridge = A11yBridge::new();
        let mut diagnostics = Vec::new();
        let snapshot = bridge
            .take(&mut tree, None, scale(), "Kytrix", &mut diagnostics)
            .expect("首笔");
        assert!(snapshot.node_for(canvas).expect("canvas").hidden);
        assert!(
            diagnostics
                .iter()
                .any(|diag| matches!(diag, crate::a11y::A11yDiagnostic::MissingAltText { node, .. } if *node == canvas)),
            "必须记 MissingAltText（不静默丢弃）"
        );
    }

    #[test]
    fn scale_change_forces_a_full_tree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, root)
            .expect("node");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(!bridge.pending(&tree));

        bridge.note_bounds_changed();
        assert!(bridge.pending(&tree), "scale 变化必须被 pending 看见");
        let full = take(&mut bridge, &mut tree, None).expect("全树");
        assert!(full.full);
    }

    #[test]
    fn scroll_invalidation_resends_the_subtree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("container");
        let row = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, container)
            .expect("row");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        bridge.invalidate_subtree(container);
        let patch = take(&mut bridge, &mut tree, None).expect("滚动增量");
        assert!(patch.node_for(container).is_some());
        assert!(patch.node_for(row).is_some(), "容器子树必须整体重发");
    }

    #[test]
    fn tree_update_carries_toolkit_info_only_on_full() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut bridge = A11yBridge::new();
        let full = take(&mut bridge, &mut tree, None).expect("首笔");
        let update = to_tree_update(&full, root);
        assert_eq!(update.tree_id, accesskit::TreeId::ROOT);
        let info = update.tree.expect("首笔必须给 TreeInfo");
        assert_eq!(info.toolkit_name.as_deref(), Some("Kytrix"));
        assert_eq!(
            info.toolkit_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            update.focus,
            AkNodeId(crate::a11y::A11yNodeId::from_engine(root).get())
        );

        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let update = to_tree_update(&patch, root);
        assert!(update.tree.is_none(), "增量不得带 TreeInfo");
        assert_eq!(
            update.focus,
            AkNodeId(crate::a11y::A11yNodeId::from_engine(root).get())
        );
    }

    #[test]
    fn actions_and_bounds_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(1.0, 2.0, 10.0, 20.0))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(SmolStr::new_static("ok")),
                actions: Some(vec![A11yAction::Click]),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("node");
        let mut bridge = A11yBridge::new();
        // 布局盒是 a11y bounds 的唯一来源；这里直接写一个已知盒（不跑布局）。
        tree.set_layout_for_test(
            node,
            crate::node::tree::LayoutBox {
                bounds: crate::math::Rect::new(1.0, 2.0, 10.0, 20.0),
                border: crate::math::Edges::ZERO,
            },
            crate::node::tree::LayoutBox::default(),
        );
        let snapshot = take(&mut bridge, &mut tree, None).expect("首笔");
        let record = snapshot.node_for(node).expect("node");
        assert_eq!(
            (
                record.bounds.x0,
                record.bounds.y0,
                record.bounds.x1,
                record.bounds.y1
            ),
            (1, 2, 11, 22)
        );
        let converted = to_accesskit(record);
        assert!(converted.supports_action(Action::Click));
        assert_eq!(
            converted.bounds(),
            Some(accesskit::Rect::new(1.0, 2.0, 11.0, 22.0))
        );
        let _ = NodeBuilder::boxed();
    }
}

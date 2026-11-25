use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use accesskit::{NodeId as AkNodeId, Rect as AkRect, TreeId, TreeInfo, TreeUpdate};
use smallvec::SmallVec;

use crate::{
    a11y::{
        A11yDiagnostic, A11yLabelSource, A11yNode, A11yNodeId, A11yRole, A11yTreeSnapshot,
        identity::A11yIds,
        mapping::{
            action_to_accesskit, live_to_accesskit, orientation_to_accesskit, role_to_accesskit,
            sort_to_accesskit,
        },
        textrun,
        tree::{
            CollectionSemanticsCache, collect_subtree, node_a11y_records_with_cache,
            push_ancestors, resolve_focus,
        },
    },
    app::App,
    math::DevicePixelRatio,
    node::{
        id::NodeId,
        tree::{Node, NodeTree},
    },
    window::WindowId,
};

pub(crate) const A11Y_BATCH_BUDGET: usize = 2048;

#[derive(Clone, Debug)]
pub(crate) struct A11yConsumerEpoch(Arc<AtomicU64>);

impl A11yConsumerEpoch {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    pub(crate) fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    #[cfg(all(test, any(windows, feature = "test-support")))]
    #[must_use]
    pub(crate) fn same_handle(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

pub(crate) struct A11yBridge {
    full_pending: bool,
    bounds_epoch: u32,
    seen_bounds_epoch: u32,
    consumer_epoch: A11yConsumerEpoch,
    seen_consumer_epoch: u64,
    subtrees: SmallVec<[NodeId; 2]>,
    bindings: Vec<NodeId>,
    binding_set: ahash::HashSet<NodeId>,
    #[cfg(feature = "test-support")]
    a11y_only_frames: usize,
    ids: A11yIds,
    tables: CollectionSemanticsCache,
    last_focus: Option<NodeId>,
    delivered: ahash::HashSet<NodeId>,
    queue: Vec<NodeId>,
    queue_cursor: usize,
    queued: ahash::HashSet<NodeId>,
    refresh: SmallVec<[NodeId; 8]>,
    #[cfg(test)]
    refresh_deferred: u64,
    tree_info_pending: bool,
    #[cfg(feature = "test-support")]
    capture: bool,
}

impl A11yBridge {
    pub(crate) fn new() -> Self {
        Self {
            full_pending: true,
            bounds_epoch: 0,
            seen_bounds_epoch: 0,
            consumer_epoch: A11yConsumerEpoch::new(),
            seen_consumer_epoch: 0,
            subtrees: SmallVec::new(),
            bindings: Vec::new(),
            binding_set: ahash::HashSet::default(),
            #[cfg(feature = "test-support")]
            a11y_only_frames: 0,
            ids: A11yIds::new(),
            tables: CollectionSemanticsCache::default(),
            last_focus: None,
            delivered: ahash::HashSet::default(),
            queue: Vec::new(),
            queue_cursor: 0,
            queued: ahash::HashSet::default(),
            refresh: SmallVec::new(),
            #[cfg(test)]
            refresh_deferred: 0,
            tree_info_pending: false,
            #[cfg(feature = "test-support")]
            capture: false,
        }
    }

    pub(crate) fn pending(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        self.full_pending
            || self.bounds_epoch != self.seen_bounds_epoch
            || self.consumer_epoch_pending()
            || !self.subtrees.is_empty()
            || tree.a11y_dirty() > 0
            || self.queue_cursor < self.queue.len()
            || !self.refresh.is_empty()
            || self.focus_changed(tree, focus)
    }

    pub(crate) fn consumer_epoch_pending(&self) -> bool {
        self.consumer_epoch.current() != self.seen_consumer_epoch
    }

    pub(crate) fn consumer_epoch(&self) -> A11yConsumerEpoch {
        self.consumer_epoch.clone()
    }

    pub(crate) fn focus_pending(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        self.focus_changed(tree, focus)
    }

    pub(crate) fn backlog(&self) -> bool {
        self.queue_cursor < self.queue.len() || !self.refresh.is_empty()
    }

    pub(crate) fn rebuild_pending(&self) -> bool {
        self.full_pending || self.bounds_epoch != self.seen_bounds_epoch
    }

    #[cfg(test)]
    pub(crate) fn refresh_backlog(&self) -> usize {
        self.refresh.len()
    }

    #[cfg(test)]
    pub(crate) fn deferred_refresh(&self) -> u64 {
        self.refresh_deferred
    }

    pub(crate) fn note_bounds_changed(&mut self) {
        self.bounds_epoch = self.bounds_epoch.wrapping_add(1);
    }

    pub(crate) fn note_binding_work(&mut self, node: NodeId) -> bool {
        if self.binding_set.insert(node) {
            self.bindings.push(node);
            true
        } else {
            false
        }
    }

    pub(crate) fn binding_work(&self) -> bool {
        !self.bindings.is_empty()
    }

    pub(crate) fn take_bindings(&mut self) -> Vec<NodeId> {
        self.binding_set.clear();
        std::mem::take(&mut self.bindings)
    }

    pub(crate) fn recycle_bindings(&mut self, mut buffer: Vec<NodeId>) {
        buffer.clear();
        self.bindings = buffer;
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn note_a11y_only_frame(&mut self) -> usize {
        self.a11y_only_frames += 1;
        self.a11y_only_frames
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn a11y_only_frames(&self) -> usize {
        self.a11y_only_frames
    }

    pub(crate) fn invalidate_subtree(&mut self, node: NodeId) {
        if !self.subtrees.contains(&node) {
            self.subtrees.push(node);
        }
    }

    pub(crate) fn force_full(&mut self) {
        self.full_pending = true;
    }

    pub(crate) fn engine_of(&self, id: A11yNodeId) -> Option<NodeId> {
        self.ids.engine_of(id)
    }

    #[cfg(test)]
    pub(crate) fn stable_of(&self, node: NodeId) -> Option<A11yNodeId> {
        self.ids.stable_of(node)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn set_capture(&mut self, on: bool) {
        if on && !self.capture {
            self.full_pending = true;
        }
        self.capture = on;
    }

    pub(crate) fn take(
        &mut self,
        tree: &mut NodeTree,
        focus: Option<NodeId>,
        scale: DevicePixelRatio,
        title: &str,
        caret: Option<(NodeId, usize)>,
        diagnostics: &mut Vec<A11yDiagnostic>,
    ) -> Option<A11yTreeSnapshot> {
        if !self.pending(tree, focus) {
            return None;
        }
        self.ids.begin_delivery();
        self.tables.clear();
        let consumer_epoch = self.consumer_epoch.current();
        let full_run = self.full_pending
            || self.bounds_epoch != self.seen_bounds_epoch
            || consumer_epoch != self.seen_consumer_epoch;
        if full_run {
            self.full_pending = false;
            self.seen_bounds_epoch = self.bounds_epoch;
            self.seen_consumer_epoch = consumer_epoch;
            self.tree_info_pending = true;
            self.delivered.clear();
            self.queue.clear();
            self.queued.clear();
            self.queue_cursor = 0;
            self.refresh.clear();
            self.ids.prune(tree);
            let mut dirty = tree.take_a11y();
            for node in dirty.iter().copied() {
                tree.clear_a11y(node);
            }
            tree.recycle(std::mem::take(&mut dirty));
            tree.for_each_dfs(|node| {
                self.queued.insert(node);
                self.queue.push(node);
            });
        }
        if !full_run {
            self.collect_incremental(tree);
        }

        let mut batch: Vec<NodeId> = Vec::with_capacity(A11Y_BATCH_BUDGET.min(self.queue.len()));
        let mut batch_set: ahash::HashSet<NodeId> = ahash::HashSet::default();
        let push =
            |node: NodeId, batch: &mut Vec<NodeId>, batch_set: &mut ahash::HashSet<NodeId>| {
                if batch_set.insert(node) {
                    batch.push(node);
                }
            };
        let mut deferred: SmallVec<[NodeId; 8]> = SmallVec::new();
        for node in std::mem::take(&mut self.refresh) {
            if batch.len() >= A11Y_BATCH_BUDGET {
                if tree.get(node).is_ok() {
                    deferred.push(node);
                }
                continue;
            }
            if tree.get(node).is_ok() {
                push(node, &mut batch, &mut batch_set);
            }
        }
        #[cfg(test)]
        {
            self.refresh_deferred = self.refresh_deferred.saturating_add(deferred.len() as u64);
        }
        let mut ancestors: SmallVec<[NodeId; 16]> = SmallVec::new();
        while batch.len() < A11Y_BATCH_BUDGET && self.queue_cursor < self.queue.len() {
            let node = self.queue[self.queue_cursor];
            self.queue_cursor += 1;
            if tree.get(node).is_err() {
                continue;
            }
            if !full_run {
                push_ancestors(tree, node, &mut ancestors);
                for ancestor in ancestors.drain(..) {
                    push(ancestor, &mut batch, &mut batch_set);
                }
            }
            push(node, &mut batch, &mut batch_set);
        }

        batch.sort_by_key(|node| depth_of(tree, *node));
        let (focus_id, missing) = resolve_focus(tree, focus);
        if let Some(missing) = missing {
            diagnostics.push(A11yDiagnostic::MissingFocusTarget { node: missing });
        }
        let mut focus_chain: SmallVec<[NodeId; 16]> = SmallVec::new();
        if !batch_set.contains(&focus_id) {
            let mut cursor = tree.get(focus_id).ok().and_then(Node::parent);
            while let Some(parent) = cursor {
                if !batch_set.contains(&parent) {
                    focus_chain.push(parent);
                }
                cursor = tree.get(parent).ok().and_then(Node::parent);
            }
            focus_chain.reverse();
        }
        let focus_related = |engine: NodeId| engine == focus_id || focus_chain.contains(&engine);
        let mut nodes = Vec::with_capacity(batch.len() + 1);
        let mut re_refresh: SmallVec<[NodeId; 8]> = SmallVec::new();
        for node in &batch {
            if let Some(mut record) = node_a11y_records_with_cache(
                tree,
                *node,
                scale,
                title,
                &mut self.ids,
                &mut self.tables,
                diagnostics,
            ) {
                let before = record.node.children.len();
                let text_run = record.text_run.as_ref().map(|run| run.id);
                record.node.children.retain(|child| {
                    Some(*child) == text_run
                        || self.ids.engine_of(*child).is_some_and(|engine| {
                            self.delivered.contains(&engine)
                                || batch_set.contains(&engine)
                                || focus_related(engine)
                        })
                });
                if record.node.children.len() != before {
                    re_refresh.push(*node);
                }
                apply_caret(&mut record.node, caret);
                nodes.push(record.node);
                if let Some(run) = record.text_run {
                    nodes.push(run);
                }
            }
        }
        for node in &batch {
            self.delivered.insert(*node);
            self.queued.remove(node);
        }
        self.refresh = deferred;
        self.refresh.extend(re_refresh);

        let mut focus_ancestors = Vec::with_capacity(focus_chain.len() + 1);
        for ancestor in &focus_chain {
            if let Some(mut record) = node_a11y_records_with_cache(
                tree,
                *ancestor,
                scale,
                title,
                &mut self.ids,
                &mut self.tables,
                diagnostics,
            ) {
                let before = record.node.children.len();
                let text_run = record.text_run.as_ref().map(|run| run.id);
                record.node.children.retain(|child| {
                    Some(*child) == text_run
                        || self.ids.engine_of(*child).is_some_and(|engine| {
                            self.delivered.contains(&engine)
                                || batch_set.contains(&engine)
                                || focus_related(engine)
                        })
                });
                if record.node.children.len() != before {
                    self.refresh.push(*ancestor);
                }
                self.delivered.insert(*ancestor);
                self.queued.remove(ancestor);
                apply_caret(&mut record.node, caret);
                focus_ancestors.push(record.node);
                if let Some(run) = record.text_run {
                    nodes.push(run);
                }
            }
        }

        if self.queue_cursor >= self.queue.len() && self.refresh.is_empty() {
            self.queue.clear();
            self.queued.clear();
            self.queue_cursor = 0;
            self.ids.prune(tree);
            if self.delivered.len() > tree.len().saturating_mul(2) {
                self.delivered.retain(|node| tree.get(*node).is_ok());
            }
        }
        let emit_tree_info = std::mem::take(&mut self.tree_info_pending);

        let mut focus_text_run = None;
        let mut focus_node =
            if let Some(record) = nodes.iter().find(|record| record.engine == focus_id) {
                record.clone()
            } else {
                let mut record = node_a11y_records_with_cache(
                    tree,
                    focus_id,
                    scale,
                    title,
                    &mut self.ids,
                    &mut self.tables,
                    diagnostics,
                )
                .expect("resolve_focus 已保证焦点节点存活");
                let before = record.node.children.len();
                let text_run = record.text_run.as_ref().map(|run| run.id);
                record.node.children.retain(|child| {
                    Some(*child) == text_run
                        || self.ids.engine_of(*child).is_some_and(|engine| {
                            self.delivered.contains(&engine) || batch_set.contains(&engine)
                        })
                });
                if record.node.children.len() != before {
                    self.refresh.push(focus_id);
                }
                focus_text_run = record.text_run;
                record.node
            };
        apply_caret(&mut focus_node, caret);
        self.delivered.insert(focus_id);
        self.queued.remove(&focus_id);
        self.last_focus = Some(focus_id);
        let root_id = self
            .ids
            .stable_of(tree.root())
            .expect("根节点必然在 id 分配器里（首笔起）");
        Some(A11yTreeSnapshot {
            full: emit_tree_info,
            root: root_id,
            nodes,
            focus: self
                .ids
                .stable_of(focus_id)
                .expect("焦点节点刚被 node_a11y 分配 id"),
            focus_node,
            focus_text_run,
            focus_ancestors,
            diagnostics: diagnostics.len(),
        })
    }

    fn focus_changed(&self, tree: &NodeTree, focus: Option<NodeId>) -> bool {
        let (resolved, _) = resolve_focus(tree, focus);
        self.last_focus != Some(resolved)
    }

    fn collect_incremental(&mut self, tree: &mut NodeTree) {
        let mut dirty = tree.take_a11y();
        let mut fresh: SmallVec<[NodeId; 32]> = SmallVec::new();
        for node in dirty.iter().copied() {
            fresh.push(node);
            tree.clear_a11y(node);
        }
        tree.recycle(std::mem::take(&mut dirty));
        let mut subtree_nodes: SmallVec<[NodeId; 16]> = SmallVec::new();
        for root in std::mem::take(&mut self.subtrees) {
            collect_subtree(tree, root, &mut subtree_nodes);
        }
        for node in subtree_nodes.drain(..) {
            fresh.push(node);
        }
        for node in fresh {
            if self.queued.insert(node) {
                self.queue.push(node);
            }
        }
    }
}

impl Default for A11yBridge {
    fn default() -> Self {
        Self::new()
    }
}

fn depth_of(tree: &NodeTree, node: NodeId) -> u32 {
    let mut depth = 0u32;
    let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
    while let Some(parent) = cursor {
        depth += 1;
        cursor = tree.get(parent).ok().and_then(|target| target.parent());
    }
    depth
}

fn apply_caret(record: &mut A11yNode, caret: Option<(NodeId, usize)>) {
    let Some((node, byte_index)) = caret else {
        return;
    };
    if record.engine != node
        || record.text_selection.is_some()
        || record.character_lengths.is_empty()
        || record.role != A11yRole::TextRun
        || record.text_run.is_some()
    {
        return;
    }
    record.text_selection = Some(textrun::selection(
        &record.character_lengths,
        crate::a11y::A11yTextSelection {
            anchor: byte_index,
            focus: byte_index,
        },
    ));
}

pub(crate) fn build_update(app: &mut App, window: WindowId) -> TreeUpdate {
    if app.window(window).is_none() {
        return TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: TreeId::ROOT,
            focus: AkNodeId(0),
        };
    }
    let Some(snapshot) = app.a11y_take(window) else {
        if let Some(target) = app.window_mut(window) {
            target.a11y_force_full();
        }
        let snapshot = app
            .a11y_take(window)
            .expect("a11y: force-full delivery must produce a snapshot");
        return to_tree_update(&snapshot);
    };
    to_tree_update(&snapshot)
}

pub(crate) fn to_tree_update(snapshot: &A11yTreeSnapshot) -> TreeUpdate {
    let tree = snapshot.full.then(|| TreeInfo {
        root: AkNodeId(snapshot.root.get()),
        toolkit_name: Some(String::from("Kytrix")),
        toolkit_version: Some(String::from(env!("CARGO_PKG_VERSION"))),
    });
    let mut nodes = Vec::with_capacity(snapshot.nodes.len() + snapshot.focus_ancestors.len() + 1);
    for record in &snapshot.nodes {
        nodes.push((AkNodeId(record.id.get()), to_accesskit(record)));
    }
    let focus_engine = snapshot.focus_node.engine;
    if !snapshot
        .nodes
        .iter()
        .any(|record| record.engine == focus_engine)
    {
        for record in &snapshot.focus_ancestors {
            nodes.push((AkNodeId(record.id.get()), to_accesskit(record)));
        }
        nodes.push((
            AkNodeId(snapshot.focus_node.id.get()),
            to_accesskit(&snapshot.focus_node),
        ));
        if let Some(run) = &snapshot.focus_text_run {
            nodes.push((AkNodeId(run.id.get()), to_accesskit(run)));
        }
    }
    TreeUpdate {
        nodes,
        tree,
        tree_id: TreeId::ROOT,
        focus: AkNodeId(snapshot.focus.get()),
    }
}

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
    if let Some(label) = &record.label {
        if record.role == A11yRole::Label
            || record.role == A11yRole::TextRun
            || label.source() == A11yLabelSource::TextContent
        {
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
    if let Some(checked) = record.checked {
        node.set_toggled(accesskit::Toggled::from(checked));
    }
    if let Some(expanded) = record.expanded {
        node.set_expanded(expanded);
    }
    if let Some(live) = record.live {
        node.set_live(live_to_accesskit(live));
    }
    if let Some(position) = record.pos_in_set {
        node.set_position_in_set(position);
    }
    if let Some(size) = record.size_of_set {
        node.set_size_of_set(size);
    }
    if let Some(value) = record.numeric_value {
        node.set_numeric_value(value);
    }
    if let Some(value) = record.min_numeric_value {
        node.set_min_numeric_value(value);
    }
    if let Some(value) = record.max_numeric_value {
        node.set_max_numeric_value(value);
    }
    if let Some(value) = record.numeric_value_step {
        node.set_numeric_value_step(value);
    }
    if let Some(value) = record.numeric_value_jump {
        node.set_numeric_value_jump(value);
    }
    if let Some(index) = record.row_index {
        node.set_row_index(index);
    }
    if let Some(index) = record.column_index {
        node.set_column_index(index);
    }
    if let Some(count) = record.row_count {
        node.set_row_count(count);
    }
    if let Some(count) = record.column_count {
        node.set_column_count(count);
    }
    if let Some(sort) = record.sort {
        node.set_sort_direction(sort_to_accesskit(sort));
    }
    if let Some(scroll) = record.scroll {
        node.set_scroll_x(f64::from(scroll.x.value()));
        node.set_scroll_y(f64::from(scroll.y.value()));
        node.set_scroll_x_min(0.0);
        node.set_scroll_y_min(0.0);
        node.set_scroll_x_max(f64::from(scroll.x_max.value()));
        node.set_scroll_y_max(f64::from(scroll.y_max.value()));
    }
    if let Some(orientation) = record.orientation {
        node.set_orientation(orientation_to_accesskit(orientation));
    } else if let Some(scroll) = record.scroll {
        node.set_orientation(if scroll.vertical {
            accesskit::Orientation::Vertical
        } else {
            accesskit::Orientation::Horizontal
        });
    }
    if record.clips_children {
        node.set_clips_children();
    }
    if !record.character_lengths.is_empty() {
        node.set_character_lengths(record.character_lengths.clone());
    }
    if !record.word_starts.is_empty() {
        node.set_word_starts(record.word_starts.clone());
    }
    if !record.character_positions.is_empty()
        && record.character_positions.len() == record.character_lengths.len()
        && record.character_widths.len() == record.character_lengths.len()
    {
        node.set_character_positions(record.character_positions.clone());
        node.set_character_widths(record.character_widths.clone());
        node.set_text_direction(accesskit::TextDirection::LeftToRight);
    }
    if let Some(selection) = record.text_selection {
        let selection_node = AkNodeId(record.text_run.unwrap_or(record.id).get());
        node.set_text_selection(accesskit::TextSelection {
            anchor: accesskit::TextPosition {
                node: selection_node,
                character_index: selection.anchor,
            },
            focus: accesskit::TextPosition {
                node: selection_node,
                character_index: selection.focus,
            },
        });
    }
    if record.read_only {
        node.set_read_only();
    }
    if record.modal {
        node.set_modal();
    }
    if record.busy {
        node.set_busy();
    }
    if record.invalid {
        node.set_invalid(accesskit::Invalid::True);
    }
    if let Some(message) = record.error_message {
        node.set_error_message(AkNodeId(message.get()));
    }
    if let Some(placeholder) = &record.placeholder {
        node.set_placeholder(placeholder.as_str());
    }
    if let Some(level) = record.level {
        node.set_level(level);
    }
    for custom in &record.custom_actions {
        node.push_custom_action(accesskit::CustomAction {
            id: custom.id,
            description: custom.label.to_string(),
        });
    }
    if !record.custom_actions.is_empty() {
        node.add_action(accesskit::Action::CustomAction);
    }
    for action in &record.actions {
        node.add_action(action_to_accesskit(*action));
    }
    let bounds = record.bounds;
    node.set_bounds(AkRect::new(
        f64::from(bounds.x0.value()),
        f64::from(bounds.y0.value()),
        f64::from(bounds.x1.value()),
        f64::from(bounds.y1.value()),
    ));
    node
}

#[cfg(test)]
mod tests {
    use accesskit::{Action, NodeId as AkNodeId};
    use smol_str::SmolStr;

    use super::{A11Y_BATCH_BUDGET, A11yBridge, to_accesskit, to_tree_update};
    use crate::{
        a11y::{
            A11yAction, A11yCustomAction, A11yOverride, A11yPatch, A11yRole, A11ySort,
            A11yTextSelection,
        },
        math::{DevicePixelRatio, dp, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style::StylePatch,
        text::TextStyle,
    };

    fn scale() -> DevicePixelRatio {
        DevicePixelRatio::ONE
    }

    fn take(
        bridge: &mut A11yBridge,
        tree: &mut NodeTree,
        focus: Option<NodeId>,
    ) -> Option<crate::a11y::A11yTreeSnapshot> {
        let mut diagnostics = Vec::new();
        bridge.take(tree, focus, scale(), "Kytrix", None, &mut diagnostics)
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
        assert_eq!(full.nodes.len(), tree.len());
        assert_eq!(full.focus, full.node_for(root).expect("root").id);
        assert_eq!(
            full.node_for(button).expect("button").role,
            A11yRole::Button
        );
        assert!(!bridge.pending(&tree, None), "交付后必须无工作");

        assert!(
            take(&mut bridge, &mut tree, None).is_none(),
            "无变化时不得产出"
        );

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
        assert_eq!(
            full.node_for(button).expect("button").id,
            patch.node_for(button).expect("button").id
        );
    }

    #[test]
    fn a_consumer_epoch_reset_forces_the_next_delivery_to_be_full() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();

        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full, "首次激活必须是全树");
        assert!(
            to_tree_update(&first).tree.is_some(),
            "全树笔必须带 TreeInfo"
        );

        tree.mark_a11y(button);
        let incremental = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(!incremental.full);
        assert!(to_tree_update(&incremental).tree.is_none());
        assert!(!bridge.pending(&tree, None), "交付后必须无工作");

        let epoch = bridge.consumer_epoch();
        epoch.bump();
        assert!(
            bridge.consumer_epoch_pending(),
            "重置后必须标记「待全树」（不依赖脏位）"
        );
        assert!(
            bridge.pending(&tree, None),
            "重置后 pending 必须为真，否则平台泵不会交付"
        );

        let forced = take(&mut bridge, &mut tree, None).expect("重置后的第一笔");
        assert!(forced.full, "重置后的第一笔必须是全树");
        assert!(
            to_tree_update(&forced).tree.is_some(),
            "AccessKit 要求重置后的首笔携带 TreeInfo（否则 consumer panic/空树）"
        );
        assert_eq!(forced.nodes.len(), tree.len(), "全树笔必须覆盖整棵树");
        assert!(
            !bridge.consumer_epoch_pending(),
            "全树交付后重置标记必须消费掉"
        );
        assert!(!bridge.pending(&tree, None), "消费后不得残留待办");
        assert!(
            take(&mut bridge, &mut tree, None).is_none(),
            "全树交付后无变化时不得再产出"
        );
    }

    #[test]
    fn two_consumer_epoch_resets_collapse_into_one_full_delivery() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");
        tree.mark_a11y(button);
        let _ = take(&mut bridge, &mut tree, None).expect("增量");

        let epoch = bridge.consumer_epoch();
        epoch.bump();
        epoch.bump();
        let forced = take(&mut bridge, &mut tree, None).expect("重置后的第一笔");
        assert!(forced.full);
        assert!(to_tree_update(&forced).tree.is_some());
        assert!(!bridge.consumer_epoch_pending(), "两次重置一次交付即消费完");
    }

    #[test]
    fn an_update_to_an_already_delivered_node_during_a_batched_run_is_redelivered() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(3000);
        for _ in 0..3000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full);
        assert_eq!(first.nodes.len(), A11Y_BATCH_BUDGET);
        assert!(first.node_for(rows[100]).is_some(), "首笔必须包含行 100");
        assert!(
            first.node_for(rows[2999]).is_none(),
            "首笔预算 2048 ⇒ 行 2999 尚未交付"
        );

        tree.set_a11y_patch(
            rows[100],
            A11yPatch {
                label: Some(SmolStr::new_static("changed")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(rows[100]);

        let second = take(&mut bridge, &mut tree, None).expect("第二笔");
        assert!(!second.full, "运行中不得退化成全树");
        assert_eq!(
            second
                .node_for(rows[100])
                .expect("已交付行被改脏后必须重发（否则更新永久丢失）")
                .label_text(),
            Some("changed")
        );
        assert!(
            second.node_for(rows[2047]).is_some(),
            "队列照常推进：重发已交付行不得挤掉尚未交付的新节点"
        );
    }

    #[test]
    fn a_subtree_invalidation_reaches_already_delivered_nodes() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("container");
        let mut rows = Vec::with_capacity(3000);
        for _ in 0..3000 {
            rows.push(
                NodeBuilder::boxed()
                    .build(&mut tree, container)
                    .expect("row"),
            );
        }
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full);
        assert!(first.node_for(rows[0]).is_some(), "行 0 已交付");
        assert!(first.node_for(rows[2999]).is_none(), "行 2999 尚未交付");

        bridge.invalidate_subtree(container);

        let second = take(&mut bridge, &mut tree, None).expect("第二笔");
        assert!(
            second.node_for(rows[0]).is_some(),
            "已交付行必须在滚动后重发（否则 AT 的 bounds 永久过期）"
        );
        assert!(
            second.node_for(container).is_some(),
            "容器自身也必须重发（children 的几何来自这里）"
        );
    }

    #[test]
    fn marks_during_a_long_batched_run_are_not_starved() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(5000);
        for _ in 0..5000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(first.full);

        let mut seen: ahash::HashMap<NodeId, Option<SmolStr>> = ahash::HashMap::default();
        for (index, row) in rows.iter().enumerate().take(100) {
            let label = smol_str::format_smolstr!("改{index}");
            tree.set_a11y_patch(
                *row,
                A11yPatch {
                    label: Some(label.clone()),
                    ..A11yPatch::default()
                },
            );
            tree.mark_a11y(*row);
            seen.insert(*row, Some(label));
        }

        let mut batches = 1usize;
        while let Some(snapshot) = take(&mut bridge, &mut tree, None) {
            batches += 1;
            assert!(
                batches < 32,
                "批次必须收敛（实际 {batches}，疑似队列膨胀死循环）"
            );
            for record in &snapshot.nodes {
                if let Some(expected) = seen.get_mut(&record.engine)
                    && record.label_text() == expected.as_deref()
                {
                    *expected = None;
                }
            }
        }
        let missing: Vec<NodeId> = seen
            .iter()
            .filter_map(|(row, expected)| expected.as_ref().map(|_| *row))
            .collect();
        assert!(
            missing.is_empty(),
            "运行期间写入的 100 个新值必须全部送达，缺失节点：{missing:?}（共 {batches} 笔）"
        );
        assert!(
            batches <= 2 * 5000_usize.div_ceil(A11Y_BATCH_BUDGET) + 8,
            "批数必须收敛在上界内（防队列膨胀活锁），实际 {batches} 笔"
        );
        assert!(!bridge.pending(&tree, None), "收敛后不得残留待办");
    }

    #[test]
    fn the_focus_node_is_carried_in_the_same_tree_update() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let full = take(&mut bridge, &mut tree, None).expect("首笔");
        let update = to_tree_update(&full);
        assert!(
            update
                .nodes
                .iter()
                .any(|(id, _)| *id == AkNodeId(full.focus.get())),
            "首笔焦点必须在本笔 nodes 里"
        );

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
        let update = to_tree_update(&patch);
        let focus = update.focus;
        assert_eq!(focus, AkNodeId(patch.focus_node.id.get()));
        assert!(
            update.nodes.iter().any(|(id, _)| *id == focus),
            "焦点节点必须与 focus 同笔送达（否则 consumer 指向不存在的节点）"
        );
        assert_eq!(
            update.nodes.iter().filter(|(id, _)| *id == focus).count(),
            1
        );
    }

    #[test]
    fn a_focus_target_that_was_never_delivered_arrives_with_the_focus() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut children = Vec::new();
        for _ in 0..6000 {
            children.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let never_delivered = children[5999];
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        assert!(first.full);
        assert!(
            first.node_for(never_delivered).is_none(),
            "首笔只发预算内前缀，最后一个节点还没被交付"
        );

        assert!(
            bridge.pending(&tree, Some(never_delivered)),
            "焦点变化必须让 bridge pending（不依赖脏位）"
        );
        let snapshot = take(&mut bridge, &mut tree, Some(never_delivered)).expect("焦点笔");
        assert!(
            snapshot.node_for(never_delivered).is_none(),
            "本批仍在推进首树前缀，焦点节点不在 nodes 里"
        );

        let update = to_tree_update(&snapshot);
        assert_eq!(update.focus, AkNodeId(snapshot.focus_node.id.get()));
        let focus_record = update
            .nodes
            .iter()
            .find(|(id, _)| *id == update.focus)
            .map(|(_, node)| node)
            .expect("焦点节点必须与 focus 同笔送达");
        assert_eq!(focus_record.role(), accesskit::Role::GenericContainer);
        assert_eq!(
            update
                .nodes
                .iter()
                .filter(|(id, _)| *id == update.focus)
                .count(),
            1,
            "焦点节点不得重复出现"
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn a_focus_target_with_a_deep_parent_chain_is_a_valid_tree_update() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(6000);
        for _ in 0..6000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let focus_parent = rows[5999];
        let focus = NodeBuilder::boxed()
            .build(&mut tree, focus_parent)
            .expect("focus node");
        let _cell = NodeBuilder::boxed().build(&mut tree, focus).expect("cell");

        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        let mut shadow = crate::test_support::a11y::ShadowTree::new();
        shadow
            .apply(&to_tree_update(&first))
            .expect("首批 TreeUpdate 必须合法");

        let snapshot = take(&mut bridge, &mut tree, Some(focus)).expect("焦点笔");
        assert!(
            snapshot.node_for(focus).is_none(),
            "前置条件：焦点节点不在本批 nodes 里"
        );
        assert!(
            !snapshot.focus_ancestors.is_empty(),
            "前置条件：焦点父链不在本批，必须走 focus_ancestors 分支"
        );
        let update = to_tree_update(&snapshot);
        crate::test_support::a11y::assert_contract_holds(&mut shadow, &update);
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn table_semantics_cache_is_invalidated_between_deliveries() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        for _ in 0..2 {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    ..A11yOverride::default()
                })
                .build(&mut tree, table)
                .expect("row");
            let _cell = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Cell),
                    ..A11yOverride::default()
                })
                .build(&mut tree, row)
                .expect("cell");
        }

        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        assert_eq!(
            first.node_for(table).and_then(|node| node.row_count),
            Some(2),
            "首批必须看到 2 行"
        );

        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .build(&mut tree, table)
            .expect("row");
        let _cell = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Cell),
                ..A11yOverride::default()
            })
            .build(&mut tree, row)
            .expect("cell");
        tree.mark_a11y(table);

        let second = take(&mut bridge, &mut tree, None).expect("增行后的交付");
        assert_eq!(
            second.node_for(table).and_then(|node| node.row_count),
            Some(3),
            "新一笔交付必须重建表格索引，不能复用 2 行的旧缓存"
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn a_focus_target_with_undelivered_children_is_a_valid_tree_update() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::with_capacity(6000);
        for _ in 0..6000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let focus = rows[5999];
        let _cell = NodeBuilder::boxed().build(&mut tree, focus).expect("cell");

        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首批");
        let mut shadow = crate::test_support::a11y::ShadowTree::new();
        shadow
            .apply(&to_tree_update(&first))
            .expect("首批 TreeUpdate 必须合法");

        let snapshot = take(&mut bridge, &mut tree, Some(focus)).expect("焦点笔");
        assert!(
            snapshot.node_for(focus).is_none(),
            "前置条件：焦点节点不在本批 nodes 里"
        );
        assert_eq!(
            tree.get(focus).expect("focus row").children().len(),
            1,
            "前置条件：焦点节点必须带一个未交付子节点"
        );
        let update = to_tree_update(&snapshot);
        crate::test_support::a11y::assert_contract_holds(&mut shadow, &update);
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
                .contains(&patch.node_for(child).expect("child").id)
        );
        assert!(patch.node_for(child).is_some(), "新子节点必须出现");

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
        assert_eq!(patch.focus, patch.node_for(button).expect("button").id);
        assert!(patch.node(patch.focus).is_some(), "焦点必须在快照里");

        tree.remove(button).expect("remove");
        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, Some(button)).expect("陈旧焦点");
        assert_eq!(
            patch.focus,
            patch.node_for(root).expect("root").id,
            "陈旧焦点回退到根"
        );
    }

    #[test]
    fn focus_changes_make_the_bridge_pending_without_dirty_bits() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed().build(&mut tree, root).expect("button");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(!bridge.pending(&tree, None));
        assert!(bridge.pending(&tree, Some(button)), "焦点变化必须 pending");
        assert!(bridge.focus_pending(&tree, Some(button)));
        let _ = take(&mut bridge, &mut tree, Some(button)).expect("焦点笔");
        assert!(
            !bridge.pending(&tree, Some(button)),
            "交付后焦点不再 pending"
        );
        assert!(!bridge.focus_pending(&tree, Some(button)));
    }

    #[test]
    fn a_large_tree_is_delivered_in_budgeted_batches() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list = NodeBuilder::boxed().build(&mut tree, root).expect("list");
        for _ in 0..5000 {
            NodeBuilder::boxed().build(&mut tree, list).expect("row");
        }
        let mut bridge = A11yBridge::new();
        let mut batches = 0usize;
        let mut seen = ahash::HashSet::default();
        while let Some(snapshot) = take(&mut bridge, &mut tree, None) {
            batches += 1;
            assert!(
                snapshot.nodes.len() <= A11Y_BATCH_BUDGET,
                "单笔节点数超预算：{} > {A11Y_BATCH_BUDGET}",
                snapshot.nodes.len()
            );
            assert!(batches < 100, "分批必须收敛（死循环保护）");
            if batches == 1 {
                assert!(snapshot.full, "首笔必须带 TreeInfo");
            }
            for record in &snapshot.nodes {
                if let Some(parent_engine) = tree
                    .get(record.engine)
                    .ok()
                    .and_then(crate::node::tree::Node::parent)
                {
                    assert!(
                        seen.contains(&parent_engine)
                            || snapshot
                                .nodes
                                .iter()
                                .any(|other| other.engine == parent_engine),
                        "节点的父必须已在树里或同批"
                    );
                }
                seen.insert(record.engine);
            }
            if !bridge.pending(&tree, None) {
                break;
            }
        }
        assert!(batches > 1, "5000 节点不可能一笔发完（实际 {batches} 笔）");
        assert_eq!(
            seen.len(),
            tree.len(),
            "分批不得丢节点（重发的父节点会重复出现，按集合计）"
        );
    }

    #[test]
    fn incremental_updates_do_not_resend_the_whole_tree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut rows = Vec::new();
        for _ in 0..2000 {
            rows.push(NodeBuilder::boxed().build(&mut tree, root).expect("row"));
        }
        let mut bridge = A11yBridge::new();
        while let Some(_snapshot) = take(&mut bridge, &mut tree, None) {
            if !bridge.pending(&tree, None) {
                break;
            }
        }
        tree.set_a11y_patch(
            rows[1000],
            A11yPatch {
                label: Some(SmolStr::new_static("changed")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(rows[1000]);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(!patch.full);
        assert!(
            patch.nodes.len() <= 2,
            "2000 节点改 1 处只需 1 节点 + 祖先，实际 {}",
            patch.nodes.len()
        );
    }

    #[test]
    fn hidden_disabled_and_patch_fields_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("node");
        let message = NodeBuilder::text("余额不足", TextStyle::new(px(12.0)))
            .build(&mut tree, root)
            .expect("message");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        tree.set_a11y_patch(
            node,
            A11yPatch {
                label: Some(SmolStr::new_static("量")),
                value: Some(SmolStr::new_static("42")),
                disabled: Some(true),
                hidden: Some(true),
                checked: Some(true),
                selected: Some(true),
                read_only: Some(true),
                busy: Some(true),
                invalid: Some(true),
                error_message: Some(message),
                placeholder: Some(SmolStr::new_static("输入价格")),
                level: Some(2),
                modal: Some(true),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(node);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let record = patch.node_for(node).expect("node");
        assert!(record.hidden);
        assert!(record.disabled);
        assert_eq!(record.checked, Some(true));
        assert_eq!(record.selected, Some(true));
        assert_eq!(record.label_text(), Some("量"));
        assert_eq!(record.value.as_deref(), Some("42"));

        let converted = to_accesskit(record);
        assert!(converted.is_hidden());
        assert!(converted.is_disabled());
        assert_eq!(converted.toggled(), Some(accesskit::Toggled::True));
        assert_eq!(converted.is_selected(), Some(true));
        assert_eq!(converted.label(), Some("量"));
        assert_eq!(converted.value(), Some("42"));
        assert!(converted.is_read_only());
        assert!(converted.is_busy());
        assert_eq!(converted.invalid(), Some(accesskit::Invalid::True));
        assert_eq!(
            converted.error_message(),
            Some(AkNodeId(
                bridge
                    .stable_of(message)
                    .expect("message 节点在首笔里已分配 id")
                    .get()
            )),
            "error_message 是 NodeId 关系（aria-errormessage）"
        );
        assert_eq!(converted.placeholder(), Some("输入价格"));
        assert_eq!(converted.level(), Some(2));
        assert!(converted.is_modal());
        let _ = StylePatch::default();
    }

    #[test]
    fn custom_actions_sort_and_scroll_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let header = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ColumnHeader),
                label: Some(SmolStr::new_static("价格")),
                sort: Some(A11ySort::Ascending),
                custom_actions: vec![A11yCustomAction {
                    id: 5,
                    label: SmolStr::new_static("按价格排序"),
                }],
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("header");
        let mut bridge = A11yBridge::new();
        let snapshot = take(&mut bridge, &mut tree, None).expect("首笔");
        let converted = to_accesskit(snapshot.node_for(header).expect("header"));
        assert_eq!(
            converted.sort_direction(),
            Some(accesskit::SortDirection::Ascending)
        );
        assert_eq!(converted.custom_actions().len(), 1);
        assert!(converted.supports_action(Action::CustomAction));
    }

    #[test]
    fn scale_change_forces_a_full_tree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::LogicalRect::new(
                px(0.0),
                px(0.0),
                px(10.0),
                px(10.0),
            ))
            .build(&mut tree, root)
            .expect("node");
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");
        assert!(!bridge.pending(&tree, None));

        bridge.note_bounds_changed();
        assert!(bridge.pending(&tree, None), "scale 变化必须被 pending 看见");
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
            .layout_override(crate::math::LogicalRect::new(
                px(0.0),
                px(0.0),
                px(10.0),
                px(10.0),
            ))
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
        let update = to_tree_update(&full);
        assert!(update.tree.is_some(), "首笔必须给 TreeInfo");
        assert_eq!(update.tree_id, accesskit::TreeId::ROOT);
        assert_eq!(
            full.focus,
            full.node_for(root).expect("root").id,
            "无焦点时焦点是根"
        );

        tree.mark_a11y(root);
        let patch = take(&mut bridge, &mut tree, None).expect("增量");
        let update = to_tree_update(&patch);
        assert!(update.tree.is_none(), "增量不得带 TreeInfo");
        assert_eq!(update.focus, AkNodeId(patch.focus.get()));
    }

    #[test]
    fn actions_and_bounds_reach_accesskit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::LogicalRect::new(
                px(1.0),
                px(2.0),
                px(10.0),
                px(20.0),
            ))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(SmolStr::new_static("ok")),
                actions: Some(vec![A11yAction::Click]),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("node");
        let mut bridge = A11yBridge::new();
        tree.set_layout_for_test(
            node,
            crate::node::tree::LayoutBox {
                bounds: crate::math::LogicalRect::new(px(1.0), px(2.0), px(10.0), px(20.0)),
                border: crate::math::LogicalEdges::ZERO,
            },
        );
        let snapshot = take(&mut bridge, &mut tree, None).expect("首笔");
        let record = snapshot.node_for(node).expect("node");
        assert_eq!(
            (dp(1), dp(2), dp(11), dp(22)),
            (
                record.bounds.x0,
                record.bounds.y0,
                record.bounds.x1,
                record.bounds.y1
            )
        );
        let converted = to_accesskit(record);
        assert!(converted.supports_action(Action::Click));
        assert_eq!(
            converted.bounds(),
            Some(accesskit::Rect::new(1.0, 2.0, 11.0, 22.0))
        );
    }

    #[test]
    fn the_ime_caret_hint_fills_the_text_selection() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");
        let text = NodeBuilder::text("你好ab", TextStyle::new(px(12.0)))
            .build(&mut tree, input)
            .expect("text");
        let mut bridge = A11yBridge::new();
        let mut diagnostics = Vec::new();
        let snapshot = bridge
            .take(
                &mut tree,
                Some(text),
                scale(),
                "Kytrix",
                Some((text, 3)),
                &mut diagnostics,
            )
            .expect("首笔");
        let record = snapshot.node_for(text).expect("text");
        assert_eq!(
            record.text_selection,
            Some(A11yTextSelection {
                anchor: 1,
                focus: 1
            }),
            "字节 3 = 第二个汉字起点 = grapheme 1"
        );
        let converted = to_accesskit(record);
        let selection = converted.text_selection().expect("text_selection");
        assert_eq!(selection.focus.character_index, 1);
        assert_eq!(selection.focus.node, AkNodeId(record.id.get()));
        assert_eq!(converted.character_lengths(), &[3, 3, 1, 1]);
    }
    #[test]
    fn the_refresh_set_is_capped_by_the_batch_budget() {
        const EXTRA: usize = 17;
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut nodes = Vec::with_capacity(A11Y_BATCH_BUDGET + EXTRA);
        for _ in 0..(A11Y_BATCH_BUDGET + EXTRA) {
            nodes.push(NodeBuilder::boxed().build(&mut tree, root).expect("node"));
        }
        let mut bridge = A11yBridge::new();
        let mut full_frames = 0usize;
        while take(&mut bridge, &mut tree, None).is_some() {
            full_frames += 1;
            assert!(full_frames < 16, "首笔全树的分批没有收敛");
        }
        assert_eq!(bridge.refresh_backlog(), 0, "首笔发完后不得有积压");
        bridge.refresh.extend(nodes.iter().copied());

        let batch = take(&mut bridge, &mut tree, None).expect("增量笔");
        assert!(
            batch.nodes.len() <= A11Y_BATCH_BUDGET,
            "单笔节点数超预算：{} > {A11Y_BATCH_BUDGET}",
            batch.nodes.len()
        );
        assert_eq!(
            bridge.refresh_backlog(),
            EXTRA,
            "超出预算的 {} 条必须留在下一笔（实际 {}）",
            EXTRA,
            bridge.refresh_backlog()
        );
        assert_eq!(
            bridge.deferred_refresh(),
            EXTRA as u64,
            "推迟条数必须被计数"
        );
        assert!(
            bridge.pending(&tree, None),
            "还有积压时 pending 必须为真，否则泵不会再发下一笔"
        );

        let rest = take(&mut bridge, &mut tree, None).expect("尾笔");
        assert_eq!(rest.nodes.len(), EXTRA, "积压必须被交付，不能丢");
        assert_eq!(bridge.refresh_backlog(), 0);
        assert!(!bridge.pending(&tree, None), "清空之后不得再有工作");
    }
    #[cfg(feature = "test-support")]
    #[test]
    fn a_virtualized_table_keeps_rows_under_the_table_across_incremental_updates() {
        use crate::{
            a11y::A11yRole,
            interaction::VirtualSlot,
            test_support::a11y::{ShadowTree, assert_contract_holds},
        };

        fn mount_row(tree: &mut NodeTree, viewport: NodeId, item: usize) -> (NodeId, NodeId) {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    label: Some(smol_str::format_smolstr!("row {item}")),
                    ..A11yOverride::default()
                })
                .build(tree, viewport)
                .expect("row");
            let column = NodeBuilder::boxed().build(tree, row).expect("column");
            let cell = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Cell),
                    label: Some(smol_str::SmolStr::new_static("42")),
                    ..A11yOverride::default()
                })
                .build(tree, column)
                .expect("cell");
            (row, cell)
        }

        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let list = NodeBuilder::boxed().build(&mut tree, table).expect("list");
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, list)
            .expect("viewport");
        let (row0, _cell0) = mount_row(&mut tree, viewport, 0);
        let (row1, _cell1) = mount_row(&mut tree, viewport, 1);
        let mut slot = VirtualSlot::new(px(20.0), 0);
        slot.count = 50;
        slot.mounted = vec![(0, row0), (1, row1)];
        tree.ensure_interaction(viewport)
            .expect("viewport interaction")
            .virtual_list = Some(slot);

        let mut bridge = A11yBridge::new();
        let mut shadow = ShadowTree::new();
        let mut stable: ahash::HashMap<NodeId, crate::a11y::A11yNodeId> = ahash::HashMap::default();
        let mut batches = 0usize;
        while let Some(snapshot) = take(&mut bridge, &mut tree, None) {
            batches += 1;
            assert!(batches < 16, "首笔全树的分批没有收敛");
            for record in &snapshot.nodes {
                stable.insert(record.engine, record.id);
            }
            assert_contract_holds(&mut shadow, &to_tree_update(&snapshot));
        }
        assert!(stable.contains_key(&table) && stable.contains_key(&row0));

        tree.set_a11y_patch(
            row0,
            A11yPatch {
                label: Some(smol_str::SmolStr::new_static("row 0（改）")),
                ..A11yPatch::default()
            },
        );
        tree.mark_a11y(row0);
        let patch = take(&mut bridge, &mut tree, None).expect("增量笔");
        assert!(!patch.full, "单行变化不得退化成全树");
        assert!(
            patch.nodes.len() <= 8,
            "增量只带变更行 + 祖先链（实际 {} 个节点）",
            patch.nodes.len()
        );
        let row_record = patch.node_for(row0).expect("变更行必须在本笔");
        let table_record = patch
            .node(row_record.parent.expect("行必须有 a11y 父"))
            .expect("逻辑父（Table）必须与变更行同批");
        assert_eq!(table_record.engine, table);
        assert_eq!(
            table_record.role,
            A11yRole::Table,
            "行的 a11y 父角色必须是 Table（不是 List 行窗口）"
        );
        assert_contract_holds(&mut shadow, &to_tree_update(&patch));

        let (row2, _cell2) = mount_row(&mut tree, viewport, 2);
        tree.virtual_slot_mut(viewport)
            .expect("virtual slot")
            .mounted
            .push((2, row2));
        let patch = take(&mut bridge, &mut tree, None).expect("挂载行笔");
        assert!(!patch.full);
        let table_record = patch.node_for(table).expect("表必须重发 children");
        let row2_record = patch.node_for(row2).expect("新行必须在本笔");
        assert!(
            table_record.children.contains(&row2_record.id),
            "新行的 a11y 父必须是表（表的 children 必须包含它）"
        );
        assert_contract_holds(&mut shadow, &to_tree_update(&patch));

        let gone = stable.get(&row1).copied();
        tree.remove(row1).expect("unmount row");
        tree.virtual_slot_mut(viewport)
            .expect("virtual slot")
            .mounted
            .retain(|(_, row)| *row != row1);
        let patch = take(&mut bridge, &mut tree, None).expect("卸载行笔");
        let table_record = patch.node_for(table).expect("表必须重发 children");
        assert!(
            !table_record
                .children
                .iter()
                .any(|child| patch.node(*child).is_some_and(|node| node.engine == row1)),
            "被卸载的行不得再出现在表的 children 里"
        );
        assert_contract_holds(&mut shadow, &to_tree_update(&patch));
        if let Some(gone) = gone {
            assert!(
                !shadow
                    .summaries()
                    .contains_key(&accesskit::NodeId(gone.get())),
                "被卸载行的 a11y 节点必须从影子树里消失"
            );
        }
    }

    #[test]
    fn the_new_roles_materialize_into_accesskit_nodes() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut ids = crate::a11y::identity::A11yIds::new();
        let mut diagnostics = Vec::new();
        let mut tables = crate::a11y::tree::CollectionSemanticsCache::default();
        for (role, expected) in [
            (A11yRole::ComboBox, accesskit::Role::ComboBox),
            (A11yRole::ListBox, accesskit::Role::ListBox),
            (
                A11yRole::MenuItemCheckBox,
                accesskit::Role::MenuItemCheckBox,
            ),
            (A11yRole::MenuItemRadio, accesskit::Role::MenuItemRadio),
            (A11yRole::ColorWell, accesskit::Role::ColorWell),
        ] {
            let node = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(role),
                    label: Some(SmolStr::new_static("控件")),
                    ..A11yOverride::default()
                })
                .build(&mut tree, root)
                .expect("node");
            tree.set_a11y_patch(
                node,
                A11yPatch {
                    checked: Some(true),
                    ..A11yPatch::default()
                },
            );
            let record = crate::a11y::tree::node_a11y_with_cache(
                &tree,
                node,
                scale(),
                "",
                &mut ids,
                &mut tables,
                &mut diagnostics,
            )
            .expect("record");
            let ak = to_accesskit(&record);
            assert_eq!(ak.role(), expected, "{role:?} 的 accesskit 角色");
            assert_eq!(
                ak.toggled(),
                Some(accesskit::Toggled::True),
                "{role:?} 的 checked 必须落进 Toggled"
            );
        }
    }

    #[test]
    fn a_color_well_reports_its_color_through_value() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let well = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ColorWell),
                label: Some(SmolStr::new_static("主题色")),
                value: Some(SmolStr::new_static("#336699")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("color well");
        let mut ids = crate::a11y::identity::A11yIds::new();
        let mut diagnostics = Vec::new();
        let mut tables = crate::a11y::tree::CollectionSemanticsCache::default();
        let record = crate::a11y::tree::node_a11y_with_cache(
            &tree,
            well,
            scale(),
            "",
            &mut ids,
            &mut tables,
            &mut diagnostics,
        )
        .expect("record");
        let ak = to_accesskit(&record);
        assert_eq!(ak.role(), accesskit::Role::ColorWell);
        assert_eq!(ak.value(), Some("#336699"));
    }

    #[test]
    fn explicit_orientation_wins_over_the_scroll_axis() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let scrolling = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ScrollView),
                label: Some(SmolStr::new_static("列表")),
                orientation: Some(crate::a11y::A11yOrientation::Horizontal),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("scroll node");
        tree.ensure_interaction(scrolling)
            .expect("interaction")
            .scroll = Some(crate::interaction::ScrollSlot {
            offset: crate::math::LogicalPoint::ZERO,
            content: crate::math::LogicalSize::new(px(10.0), px(100.0)),
            axis: crate::interaction::ScrollAxis::Vertical,
            line_height: px(40.0),
        });
        let mut ids = crate::a11y::identity::A11yIds::new();
        let mut diagnostics = Vec::new();
        let mut tables = crate::a11y::tree::CollectionSemanticsCache::default();
        let record = crate::a11y::tree::node_a11y_with_cache(
            &tree,
            scrolling,
            scale(),
            "",
            &mut ids,
            &mut tables,
            &mut diagnostics,
        )
        .expect("record");
        assert_eq!(
            record.orientation,
            Some(crate::a11y::A11yOrientation::Horizontal)
        );
        assert_eq!(
            to_accesskit(&record).orientation(),
            Some(accesskit::Orientation::Horizontal),
            "显式声明必须胜过 scroll.vertical 推导"
        );

        let derived = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ScrollView),
                label: Some(SmolStr::new_static("列表")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("derived scroll node");
        tree.ensure_interaction(derived)
            .expect("interaction")
            .scroll = Some(crate::interaction::ScrollSlot {
            offset: crate::math::LogicalPoint::ZERO,
            content: crate::math::LogicalSize::new(px(10.0), px(100.0)),
            axis: crate::interaction::ScrollAxis::Vertical,
            line_height: px(40.0),
        });
        let record = crate::a11y::tree::node_a11y_with_cache(
            &tree,
            derived,
            scale(),
            "",
            &mut ids,
            &mut tables,
            &mut diagnostics,
        )
        .expect("record");
        assert_eq!(record.orientation, None);
        assert_eq!(
            to_accesskit(&record).orientation(),
            Some(accesskit::Orientation::Vertical),
            "滚动容器无显式方向 ⇒ 按 vertical 轴推导"
        );
    }

    fn editable_spec() -> crate::text_edit::EditableSpec {
        crate::text_edit::EditableSpec {
            role: A11yRole::TextInput,
            label: SmolStr::new_static("搜索"),
            placeholder: SmolStr::new_static(""),
            multiline: false,
            masked: false,
            readonly: false,
            disabled: false,
            max_length: None,
            submit: None,
            text_style: TextStyle::new(px(12.0)),
            caret_color: crate::paint::color::Color::BLACK,
            selection_color: crate::paint::color::Color::BLACK,
            placeholder_color: crate::paint::color::Color::BLACK,
            composition_color: crate::paint::color::Color::BLACK,
        }
    }

    fn write_editable_patch(
        tree: &mut NodeTree,
        node: NodeId,
        value: &str,
        selection: A11yTextSelection,
    ) {
        crate::a11y::tree::write_a11y_patch(
            tree,
            node,
            A11yPatch {
                role: Some(A11yRole::TextInput),
                value: Some(SmolStr::new(value)),
                selection: Some(selection),
                ..A11yPatch::default()
            },
        );
    }

    #[test]
    fn an_editable_input_delivers_its_text_run_directly_after_it() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::editable_text(editable_spec())
            .build(&mut tree, root)
            .expect("editable input");
        write_editable_patch(
            &mut tree,
            input,
            "价🌍ab",
            A11yTextSelection {
                anchor: 3,
                focus: 8,
            },
        );
        let mut bridge = A11yBridge::new();
        let snapshot = take(&mut bridge, &mut tree, None).expect("首笔");
        let index = snapshot
            .nodes
            .iter()
            .position(|record| record.engine == input && record.text_run.is_some())
            .expect("输入节点");
        let input_record = &snapshot.nodes[index];
        let run_record = &snapshot.nodes[index + 1];
        assert_eq!(
            input_record.text_run,
            Some(run_record.id),
            "输入节点后必须紧跟它的 TextRun 子节点"
        );
        assert_eq!(run_record.engine, input);
        assert_eq!(run_record.parent, Some(input_record.id));
        assert_eq!(run_record.role, A11yRole::TextRun);
        assert_eq!(run_record.value.as_deref(), Some("价🌍ab"));
        assert_eq!(run_record.character_lengths, vec![3, 4, 1, 1]);
        assert_eq!(input_record.children.first().copied(), Some(run_record.id));
        assert_eq!(
            input_record.text_selection,
            Some(A11yTextSelection {
                anchor: 1,
                focus: 3
            }),
            "字节 3..8 → grapheme 1..3"
        );
        let update = to_tree_update(&snapshot);
        assert!(
            update
                .nodes
                .iter()
                .any(|(id, _)| *id == AkNodeId(input_record.id.get()))
                && update
                    .nodes
                    .iter()
                    .any(|(id, _)| *id == AkNodeId(run_record.id.get())),
            "输入与 TextRun 必须同笔交付"
        );
        assert_eq!(
            snapshot.node_for(input).map(|record| record.id),
            Some(input_record.id)
        );
        assert_eq!(snapshot.id_for_engine(input), Some(input_record.id));
    }

    #[test]
    fn incremental_editable_updates_resend_the_text_run_with_new_content() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::editable_text(editable_spec())
            .build(&mut tree, root)
            .expect("editable input");
        write_editable_patch(
            &mut tree,
            input,
            "ab",
            A11yTextSelection {
                anchor: 0,
                focus: 0,
            },
        );
        let mut bridge = A11yBridge::new();
        let _ = take(&mut bridge, &mut tree, None).expect("首笔");

        write_editable_patch(
            &mut tree,
            input,
            "abc",
            A11yTextSelection {
                anchor: 3,
                focus: 3,
            },
        );
        let snapshot = take(&mut bridge, &mut tree, None).expect("增量");
        assert!(!snapshot.full);
        let records: Vec<&crate::a11y::A11yNode> = snapshot
            .nodes
            .iter()
            .filter(|record| record.engine == input || record.text_run.is_some())
            .collect();
        let run = snapshot
            .nodes
            .iter()
            .find(|record| record.role == A11yRole::TextRun)
            .expect("TextRun 必须随增量重发");
        assert_eq!(run.value.as_deref(), Some("abc"));
        assert_eq!(run.character_lengths, vec![1, 1, 1]);
        assert!(records.iter().any(|record| {
            record.text_run == Some(run.id)
                && record.text_selection
                    == Some(A11yTextSelection {
                        anchor: 3,
                        focus: 3,
                    })
        }));
    }

    struct NoopChangeHandler;

    impl accesskit_consumer::TreeChangeHandler for NoopChangeHandler {
        fn node_added(&mut self, _node: &accesskit_consumer::NodeRef) {}

        fn node_updated(
            &mut self,
            _old_node: &accesskit_consumer::NodeRef,
            _new_node: &accesskit_consumer::NodeRef,
        ) {
        }

        fn focus_moved(
            &mut self,
            _old_node: Option<&accesskit_consumer::NodeRef>,
            _new_node: Option<&accesskit_consumer::NodeRef>,
        ) {
        }

        fn node_removed(&mut self, _node: &accesskit_consumer::NodeRef) {}
    }

    fn consumer(
        state: &accesskit_consumer::TreeState,
        id: crate::a11y::A11yNodeId,
    ) -> accesskit_consumer::NodeRef<'_> {
        state
            .node_by_tree_local_id(AkNodeId(id.get()), accesskit::TreeId::ROOT)
            .expect("a11y node must exist in the consumer tree")
    }

    #[test]
    fn a_focus_target_beyond_the_batch_keeps_its_text_run_attached() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        for _ in 0..6000 {
            NodeBuilder::boxed().build(&mut tree, root).expect("box");
        }
        let input = NodeBuilder::editable_text(editable_spec())
            .build(&mut tree, root)
            .expect("editable input");
        write_editable_patch(
            &mut tree,
            input,
            "价🌍ab",
            A11yTextSelection {
                anchor: 3,
                focus: 8,
            },
        );
        let mut bridge = A11yBridge::new();
        let first = take(&mut bridge, &mut tree, None).expect("首笔");
        let mut consumer_tree = accesskit_consumer::Tree::new(to_tree_update(&first), true);
        assert!(
            first.node_for(input).is_none(),
            "前置条件：可编辑输入在首笔预算之外"
        );

        let second = take(&mut bridge, &mut tree, Some(input)).expect("焦点笔");
        assert!(
            second.node_for(input).is_none(),
            "焦点笔仍在推进首笔前缀，输入只走 focus 回退"
        );
        let record = &second.focus_node;
        assert_eq!(record.engine, input, "焦点回退必须交付可编辑输入");
        let run = second
            .focus_text_run
            .as_ref()
            .expect("焦点回退必须附带虚拟 TextRun");
        assert_eq!(record.text_run, Some(run.id));
        assert!(
            record.children.contains(&run.id),
            "焦点回退的 children 必须保留本节点自己的 TextRun：{:?}",
            record.children
        );

        consumer_tree.update_and_process_changes(to_tree_update(&second), &mut NoopChangeHandler);
        let state = consumer_tree.state();
        let input_node = consumer(state, record.id);
        let run_node = consumer(state, run.id);
        assert!(input_node.supports_text_ranges());
        assert_eq!(run_node.parent().expect("run parent").id(), input_node.id());
        let selection = input_node.text_selection().expect("selection resolves");
        assert_eq!(selection.text(), "🌍a");
        assert_eq!(
            consumer(state, run.id).data().character_lengths(),
            &[3, 4, 1, 1]
        );
    }

    #[test]
    fn the_ime_caret_hint_never_fills_a_virtual_text_run() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::editable_text(editable_spec())
            .build(&mut tree, root)
            .expect("editable input");
        crate::a11y::tree::write_a11y_patch(
            &mut tree,
            input,
            A11yPatch {
                role: Some(A11yRole::TextInput),
                value: Some(SmolStr::new_static("abcd")),
                selection: None,
                ..A11yPatch::default()
            },
        );
        let mut bridge = A11yBridge::new();
        let mut diagnostics = Vec::new();
        let snapshot = bridge
            .take(
                &mut tree,
                Some(input),
                scale(),
                "Kytrix",
                Some((input, 2)),
                &mut diagnostics,
            )
            .expect("首笔");
        let record = snapshot.node_for(input).expect("input record");
        assert_eq!(
            record.text_selection, None,
            "caret hint 只服务 real TextRun；可编辑节点由 patch 提供选区"
        );
        let run = record.text_run.expect("virtual run");
        let run_record = snapshot.node(run).expect("run record");
        assert_eq!(
            run_record.text_selection, None,
            "虚拟 TextRun 不得被 caret hint 写 self-referencing 选区"
        );
        assert!(to_accesskit(record).text_selection().is_none());
    }
}

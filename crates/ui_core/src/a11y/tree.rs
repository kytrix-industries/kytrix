use smallvec::SmallVec;

use crate::{
    a11y::{
        A11yDiagnostic, A11yLabel, A11yLabelSource, A11yNode, A11yOverride, A11yPatch, A11yRole,
        A11yScroll,
        identity::{A11yIds, A11yKey},
        mapping::{
            a11y_disabled, a11y_read_only, bounds_of, effective_actions, effective_description,
            effective_label, effective_role, effective_value, is_decorative_image, is_hidden,
        },
        textrun,
    },
    interaction::ScrollAxis,
    math::Scale,
    node::{
        dependency::ReadCtx,
        id::NodeId,
        tree::{Node, NodeKind, NodeTree},
    },
    paint::style::Clip,
};

pub(crate) struct NodeA11y {
    pub over: Option<A11yOverride>,
    pub bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>>,
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

#[cfg(test)]
pub(crate) fn node_a11y(
    tree: &NodeTree,
    node: NodeId,
    scale: Scale,
    title: &str,
    ids: &mut A11yIds,
    diagnostics: &mut Vec<A11yDiagnostic>,
) -> Option<A11yNode> {
    let mut tables = CollectionSemanticsCache::default();
    node_a11y_with_cache(tree, node, scale, title, ids, &mut tables, diagnostics)
}

pub(crate) fn node_a11y_with_cache(
    tree: &NodeTree,
    node: NodeId,
    scale: Scale,
    title: &str,
    ids: &mut A11yIds,
    tables: &mut CollectionSemanticsCache,
    diagnostics: &mut Vec<A11yDiagnostic>,
) -> Option<A11yNode> {
    let target = tree.get(node).ok()?;
    let slot = tree.a11y_of(node);
    let over = slot.and_then(|slot| slot.over.as_ref());
    let patch = slot.map(|slot| &slot.patch);

    let role = effective_role(tree, node, target, over, patch);
    let lifted_window = if matches!(
        role,
        A11yRole::Table | A11yRole::Grid | A11yRole::Tree | A11yRole::ListBox | A11yRole::Menu
    ) {
        lifted_scroll_window(tree, node)
    } else {
        None
    };
    let parent = logical_a11y_parent(tree, node);
    let mut label = effective_label(target, role, over, patch);
    if node == tree.root() && label.is_none() && !title.is_empty() {
        label = Some(A11yLabel::new(
            smol_str::SmolStr::new(title),
            A11yLabelSource::Override,
        ));
    }
    let value = effective_value(target, role, over, patch);
    let description = effective_description(over, patch);
    let checked = patch.and_then(|patch| patch.checked);
    let hidden = is_hidden(target, role, label.as_ref(), over, patch)
        || hidden_by_skipped_ancestors(tree, node, parent)
        || hidden_outside_modal_scope(tree, node);
    let disabled = a11y_disabled(tree, node);
    let read_only = a11y_read_only(tree, node);
    let actions = effective_actions(tree, node, role, over, patch, disabled, read_only);

    let decorative_image = is_decorative_image(target, over);
    if role == A11yRole::Image
        && label.is_none()
        && let Some(kind) = match target.kind() {
            NodeKind::Canvas(_) => Some(crate::a11y::A11yDrawKind::Canvas),
            NodeKind::Geometry(_) => Some(crate::a11y::A11yDrawKind::Geometry),
            NodeKind::Image(_) if !decorative_image => Some(crate::a11y::A11yDrawKind::Image),
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
                | A11yRole::MenuItemCheckBox
                | A11yRole::MenuItemRadio
                | A11yRole::ListBoxOption
                | A11yRole::ScrollBar
                | A11yRole::Menu
                | A11yRole::MenuBar
                | A11yRole::ComboBox
                | A11yRole::ListBox
                | A11yRole::ColorWell
                | A11yRole::Tooltip
        )
    {
        diagnostics.push(A11yDiagnostic::MissingLabel { node });
    }

    let custom_actions = match patch.and_then(|patch| patch.custom_actions.clone()) {
        Some(actions) => actions,
        None => over
            .map(|over| over.custom_actions.clone())
            .unwrap_or_default(),
    };
    for action in &actions {
        if let crate::a11y::A11yAction::Custom(id) = action
            && !custom_actions.iter().any(|custom| custom.id == *id)
        {
            diagnostics.push(A11yDiagnostic::UnregisteredCustomAction { node, id: *id });
        }
    }

    let text_metrics = match role {
        A11yRole::Label | A11yRole::TextRun => value.as_deref().and_then(textrun::text_metrics),
        _ => None,
    };
    let (character_lengths, word_starts) = text_metrics.unwrap_or_default();
    let (character_positions, character_widths) = match (role, value.as_deref()) {
        (A11yRole::TextRun, Some(value)) => {
            textrun::character_metrics(tree, node, value, &character_lengths, scale)
                .unwrap_or_default()
        }
        _ => (Vec::new(), Vec::new()),
    };
    let text_selection = match role {
        A11yRole::TextRun => patch
            .and_then(|patch| patch.selection)
            .or_else(|| over.and_then(|over| over.selection))
            .map(|selection| textrun::selection(&character_lengths, selection)),
        _ => None,
    };

    let table = derive_table_semantics(tree, node, role, over, patch, tables);
    let sort = patch
        .and_then(|patch| patch.sort)
        .or_else(|| over.and_then(|over| over.sort));

    let scroll = scroll_state(tree, node)
        .or_else(|| lifted_window.and_then(|window| scroll_state(tree, window)));
    let clips_children = clips_children_of(tree, node) || lifted_window.is_some();

    let modal =
        patch
            .and_then(|patch| patch.modal)
            .unwrap_or_else(|| match tree.overlay_slot(node) {
                Some(slot) => slot.trap_focus,
                None => over.is_some_and(|over| over.modal),
            });
    let busy = patch
        .and_then(|patch| patch.busy)
        .or_else(|| over.map(|over| over.busy))
        .unwrap_or(false);
    let invalid = patch
        .and_then(|patch| patch.invalid)
        .or_else(|| over.map(|over| over.invalid))
        .unwrap_or(false);
    let error_message = patch
        .and_then(|patch| patch.error_message)
        .or_else(|| over.and_then(|over| over.error_message))
        .filter(|target| tree.get(*target).is_ok())
        .map(|target| {
            let key = identity_key(
                tree,
                target,
                over_child(tree, target),
                patch_child(tree, target),
            );
            ids.id_for(tree, target, key, diagnostics)
        });
    let placeholder = patch
        .and_then(|patch| patch.placeholder.clone())
        .or_else(|| over.and_then(|over| over.placeholder.clone()));
    let level = patch
        .and_then(|patch| patch.level)
        .or_else(|| over.and_then(|over| over.level));
    let orientation = patch
        .and_then(|patch| patch.orientation)
        .or_else(|| over.and_then(|over| over.orientation));

    let id = ids.id_for(
        tree,
        node,
        identity_key(tree, node, over, patch),
        diagnostics,
    );
    let children = a11y_children(tree, node, role, tables)
        .into_iter()
        .map(|child| {
            let key = identity_key(
                tree,
                child,
                over_child(tree, child),
                patch_child(tree, child),
            );
            ids.id_for(tree, child, key, diagnostics)
        })
        .collect();
    Some(A11yNode {
        id,
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
        live: patch
            .and_then(|patch| patch.live)
            .or_else(|| over.and_then(|over| over.live)),
        pos_in_set: patch
            .and_then(|patch| patch.pos_in_set)
            .or_else(|| over.and_then(|over| over.pos_in_set))
            .or_else(|| virtual_set_position(tree, node).map(|(index, _)| index)),
        size_of_set: size_of_set_of(tree, node)
            .or_else(|| lifted_window.and_then(|window| size_of_set_of(tree, window))),
        numeric_value: patch
            .and_then(|patch| patch.numeric_value)
            .or_else(|| over.and_then(|over| over.numeric_value)),
        min_numeric_value: patch
            .and_then(|patch| patch.min_numeric_value)
            .or_else(|| over.and_then(|over| over.min_numeric_value)),
        max_numeric_value: patch
            .and_then(|patch| patch.max_numeric_value)
            .or_else(|| over.and_then(|over| over.max_numeric_value)),
        numeric_value_step: patch
            .and_then(|patch| patch.numeric_value_step)
            .or_else(|| over.and_then(|over| over.numeric_value_step)),
        numeric_value_jump: patch
            .and_then(|patch| patch.numeric_value_jump)
            .or_else(|| over.and_then(|over| over.numeric_value_jump)),
        row_index: table.row_index,
        column_index: table.column_index,
        row_count: table.row_count,
        column_count: table.column_count,
        sort,
        scroll,
        clips_children,
        custom_actions,
        character_lengths,
        word_starts,
        character_positions,
        character_widths,
        text_selection,
        read_only,
        modal,
        busy,
        invalid,
        error_message,
        placeholder,
        level,
        orientation,
        children,
        parent: parent.map(|parent| {
            let key = identity_key(
                tree,
                parent,
                over_child(tree, parent),
                patch_child(tree, parent),
            );
            ids.id_for(tree, parent, key, diagnostics)
        }),
    })
}

fn over_child(tree: &NodeTree, node: NodeId) -> Option<&A11yOverride> {
    tree.a11y_of(node).and_then(|slot| slot.over.as_ref())
}

fn patch_child(tree: &NodeTree, node: NodeId) -> Option<&A11yPatch> {
    tree.a11y_of(node).map(|slot| &slot.patch)
}

fn identity_key(
    tree: &NodeTree,
    node: NodeId,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> Option<A11yKey> {
    if let Some(key) = patch
        .and_then(|patch| patch.key)
        .or_else(|| over.and_then(|over| over.key))
    {
        return Some(A11yKey::App(key));
    }
    let parent = tree.get(node).ok()?.parent()?;
    let slot = tree.virtual_slot(parent)?;
    let (index, _) = slot.mounted.iter().find(|(_, row)| *row == node)?;
    Some(A11yKey::VirtualRow {
        list: parent,
        index: *index,
    })
}

fn virtual_set_position(tree: &NodeTree, node: NodeId) -> Option<(usize, usize)> {
    let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        if let Some(slot) = tree.virtual_slot(current)
            && let Some((index, _)) = slot.mounted.iter().find(|(_, row)| *row == node)
        {
            return Some((*index, slot.count));
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    None
}

fn size_of_set_of(tree: &NodeTree, node: NodeId) -> Option<usize> {
    let slot = tree.a11y_of(node);
    slot.and_then(|slot| slot.patch.size_of_set)
        .or_else(|| {
            slot.and_then(|slot| slot.over.as_ref())
                .and_then(|over| over.size_of_set)
        })
        .or_else(|| tree.virtual_slot(node).map(|slot| slot.count))
}

fn clips_children_of(tree: &NodeTree, node: NodeId) -> bool {
    tree.scroll_slot(node).is_some()
        || tree
            .get(node)
            .is_ok_and(|state| state.style().clip != Clip::None)
}

fn scroll_state(tree: &NodeTree, node: NodeId) -> Option<A11yScroll> {
    let slot = tree.scroll_slot(node)?;
    let viewport = crate::interaction::scroll::viewport(tree, node);
    let max = crate::interaction::scroll::max_offset(&slot, viewport.size);
    Some(A11yScroll {
        x: slot.offset.x,
        y: slot.offset.y,
        x_max: max.x,
        y_max: max.y,
        vertical: matches!(slot.axis, ScrollAxis::Vertical),
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TableSemantics {
    row_index: Option<usize>,
    column_index: Option<usize>,
    row_count: Option<usize>,
    column_count: Option<usize>,
}

#[derive(Default)]
pub(crate) struct CollectionSemanticsCache {
    tables: ahash::HashMap<NodeId, TableIndex>,
    trees: ahash::HashMap<NodeId, TreeIndex>,
    #[cfg(test)]
    builds: u64,
}

#[derive(Default)]
struct TableIndex {
    row_count: Option<usize>,
    column_count: Option<usize>,
    children: Vec<NodeId>,
    rows: Vec<RowEntry>,
    row_of: ahash::HashMap<NodeId, usize>,
}

struct RowEntry {
    index: usize,
    children: Vec<NodeId>,
    column_of: ahash::HashMap<NodeId, usize>,
}

#[derive(Default)]
struct TreeIndex {
    children: ahash::HashMap<NodeId, Vec<NodeId>>,
}

impl CollectionSemanticsCache {
    pub(crate) fn clear(&mut self) {
        self.tables.clear();
        self.trees.clear();
    }

    #[cfg(test)]
    pub(crate) fn builds(&self) -> u64 {
        self.builds
    }

    fn table(&mut self, tree: &NodeTree, table: NodeId) -> &TableIndex {
        self.tables.entry(table).or_insert_with(|| {
            #[cfg(test)]
            {
                self.builds = self.builds.saturating_add(1);
            }
            build_table_index(tree, table)
        })
    }

    fn tree(&mut self, tree: &NodeTree, root: NodeId) -> &TreeIndex {
        self.trees.entry(root).or_insert_with(|| {
            #[cfg(test)]
            {
                self.builds = self.builds.saturating_add(1);
            }
            build_tree_index(tree, root)
        })
    }
}

fn build_table_index(tree: &NodeTree, table: NodeId) -> TableIndex {
    let mut index = TableIndex::default();
    let mut next_row = 0usize;
    let mut stack: Vec<NodeId> = children_of(tree, table).iter().rev().copied().collect();
    while let Some(node) = stack.pop() {
        if let Some(slot) = tree.virtual_slot(node) {
            let row_window = slot
                .mounted
                .iter()
                .all(|(_, row)| declared_role(tree, *row) == Some(A11yRole::Row));
            if row_window {
                let base = next_row;
                next_row = next_row.saturating_add(slot.count);
                if tree.get(node).ok().and_then(Node::parent) == Some(table) {
                    index.children.push(node);
                }
                for (item, row) in &slot.mounted {
                    let entry = build_row_entry(tree, *row, base.saturating_add(*item));
                    index.row_of.insert(*row, index.rows.len());
                    index.rows.push(entry);
                    index.children.push(*row);
                }
                continue;
            }
        }
        match declared_role(tree, node) {
            Some(A11yRole::Table | A11yRole::Grid) => {
                if tree.get(node).ok().and_then(Node::parent) == Some(table) {
                    index.children.push(node);
                }
                continue;
            }
            Some(A11yRole::Row) => {
                let entry = build_row_entry(tree, node, next_row);
                next_row = next_row.saturating_add(1);
                index.row_of.insert(node, index.rows.len());
                index.rows.push(entry);
                index.children.push(node);
                continue;
            }
            _ => {}
        }
        if logical_a11y_parent(tree, node) == Some(table) {
            index.children.push(node);
        }
        stack.extend(children_of(tree, node).iter().rev().copied());
    }
    index.row_count = (next_row > 0).then_some(next_row);
    index.column_count = index
        .rows
        .iter()
        .map(|row| row.column_of.len())
        .max()
        .filter(|count| *count > 0);
    index
}

fn build_row_entry(tree: &NodeTree, row: NodeId, index: usize) -> RowEntry {
    let mut entry = RowEntry {
        index,
        children: Vec::new(),
        column_of: ahash::HashMap::default(),
    };
    let mut stack: Vec<NodeId> = children_of(tree, row).iter().rev().copied().collect();
    while let Some(node) = stack.pop() {
        match declared_role(tree, node) {
            Some(A11yRole::Cell | A11yRole::ColumnHeader | A11yRole::RowHeader) => {
                let column = entry.column_of.len();
                entry.column_of.insert(node, column);
                entry.children.push(node);
                continue;
            }
            Some(A11yRole::Row | A11yRole::Table | A11yRole::Grid | A11yRole::Tree) => continue,
            _ => {}
        }
        if logical_a11y_parent(tree, node) == Some(row) {
            entry.children.push(node);
        }
        stack.extend(children_of(tree, node).iter().rev().copied());
    }
    entry
}

fn build_tree_index(tree: &NodeTree, root: NodeId) -> TreeIndex {
    let mut index = TreeIndex::default();
    let mut stack: Vec<(NodeId, NodeId)> = children_of(tree, root)
        .iter()
        .rev()
        .map(|child| (*child, root))
        .collect();
    while let Some((node, logical_parent)) = stack.pop() {
        let next_parent = match declared_role(tree, node) {
            Some(A11yRole::TreeItem) => {
                index.children.entry(logical_parent).or_default().push(node);
                node
            }
            Some(A11yRole::Tree) => {
                if tree.get(node).ok().and_then(Node::parent) == Some(logical_parent) {
                    index.children.entry(logical_parent).or_default().push(node);
                }
                continue;
            }
            _ => {
                if tree.get(node).ok().and_then(Node::parent) == Some(logical_parent) {
                    index.children.entry(logical_parent).or_default().push(node);
                }
                logical_parent
            }
        };
        for child in children_of(tree, node).iter().rev() {
            stack.push((*child, next_parent));
        }
    }
    index
}

fn declared_role(tree: &NodeTree, node: NodeId) -> Option<A11yRole> {
    let slot = tree.a11y_of(node)?;
    slot.patch
        .role
        .or_else(|| slot.over.as_ref().and_then(|over| over.role))
}

fn nearest_ancestor_where(
    tree: &NodeTree,
    node: NodeId,
    predicate: impl Fn(A11yRole) -> bool,
) -> Option<NodeId> {
    let mut cursor = tree.get(node).ok().and_then(Node::parent);
    while let Some(current) = cursor {
        if declared_role(tree, current).is_some_and(&predicate) {
            return Some(current);
        }
        cursor = tree.get(current).ok().and_then(Node::parent);
    }
    None
}

pub(crate) fn nearest_collection_ancestor(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
    nearest_ancestor_where(tree, node, |role| {
        matches!(role, A11yRole::Table | A11yRole::Grid | A11yRole::Tree)
    })
}

fn nearest_declared_container(tree: &NodeTree, node: NodeId) -> Option<(NodeId, A11yRole)> {
    let mut cursor = tree.get(node).ok().and_then(Node::parent);
    while let Some(current) = cursor {
        if let Some(
            role @ (A11yRole::Table
            | A11yRole::Grid
            | A11yRole::Tree
            | A11yRole::ListBox
            | A11yRole::Menu),
        ) = declared_role(tree, current)
        {
            return Some((current, role));
        }
        cursor = tree.get(current).ok().and_then(Node::parent);
    }
    None
}

pub(crate) fn is_suppressed_list_window(tree: &NodeTree, node: NodeId) -> bool {
    if tree.virtual_slot(node).is_none() {
        return false;
    }
    let Some((_, owner_role)) = nearest_declared_container(tree, node) else {
        return false;
    };
    let items_all_declare = |expected: A11yRole| {
        tree.virtual_slot(node).is_some_and(|slot| {
            slot.mounted
                .iter()
                .all(|(_, row)| declared_role(tree, *row) == Some(expected))
        })
    };
    match owner_role {
        A11yRole::ListBox | A11yRole::Menu => true,
        A11yRole::Table | A11yRole::Grid => items_all_declare(A11yRole::Row),
        A11yRole::Tree => items_all_declare(A11yRole::TreeItem),
        _ => false,
    }
}

pub(crate) fn lifted_scroll_root(tree: &NodeTree, window: NodeId) -> Option<NodeId> {
    if tree.virtual_slot(window).is_none()
        || tree.scroll_slot(window).is_none()
        || declared_role(tree, window).is_some()
        || !is_suppressed_list_window(tree, window)
    {
        return None;
    }
    let (root, _) = nearest_declared_container(tree, window)?;
    (tree.scroll_slot(root).is_none()).then_some(root)
}

pub(crate) fn lifted_scroll_window(tree: &NodeTree, root: NodeId) -> Option<NodeId> {
    if tree.scroll_slot(root).is_some()
        || !matches!(
            declared_role(tree, root),
            Some(
                A11yRole::Table
                    | A11yRole::Grid
                    | A11yRole::Tree
                    | A11yRole::ListBox
                    | A11yRole::Menu
            )
        )
    {
        return None;
    }
    tree.virtual_containers()
        .iter()
        .copied()
        .find(|window| lifted_scroll_root(tree, *window) == Some(root))
}

pub(crate) fn a11y_scroll_target(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
    if tree.scroll_slot(node).is_some() {
        return Some(node);
    }
    if let Some(window) = lifted_scroll_window(tree, node) {
        return Some(window);
    }
    crate::interaction::scroll::scroll_ancestor(tree, node).map(|(container, _)| container)
}

pub(crate) fn write_a11y_patch(
    tree: &mut NodeTree,
    node: NodeId,
    patch: crate::a11y::A11yPatch,
) -> bool {
    let previous = tree
        .a11y_of(node)
        .map_or_else(crate::a11y::A11yPatch::default, |slot| slot.patch.clone());
    if patch == previous {
        return false;
    }
    let affects_descendants = patch.affects_descendants(&previous);
    tree.set_a11y_patch(node, patch);
    tree.mark_a11y(node);
    if affects_descendants {
        tree.mark_subtree_a11y(node);
    }
    true
}

pub(crate) fn logical_a11y_parent(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
    let physical = tree.get(node).ok().and_then(Node::parent);
    let adopted = match declared_role(tree, node) {
        Some(A11yRole::Row) => nearest_ancestor_where(tree, node, |role| {
            matches!(role, A11yRole::Table | A11yRole::Grid)
        }),
        Some(A11yRole::Cell | A11yRole::ColumnHeader | A11yRole::RowHeader) => {
            nearest_ancestor_where(tree, node, |role| role == A11yRole::Row)
        }
        Some(A11yRole::TreeItem) => nearest_ancestor_where(tree, node, |role| {
            matches!(role, A11yRole::Tree | A11yRole::TreeItem)
        }),
        _ => None,
    };
    adopted.or(physical)
}

fn tree_collection_root(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
    let mut outermost_item = None;
    let mut cursor = tree.get(node).ok().and_then(Node::parent);
    while let Some(current) = cursor {
        match declared_role(tree, current) {
            Some(A11yRole::Tree) => return Some(current),
            Some(A11yRole::TreeItem) => outermost_item = Some(current),
            _ => {}
        }
        cursor = tree.get(current).ok().and_then(Node::parent);
    }
    outermost_item
}

fn a11y_children(
    tree: &NodeTree,
    node: NodeId,
    role: A11yRole,
    tables: &mut CollectionSemanticsCache,
) -> Vec<NodeId> {
    match role {
        A11yRole::Table | A11yRole::Grid => tables.table(tree, node).children.clone(),
        A11yRole::Row => {
            let Some(table) = nearest_ancestor_where(tree, node, |role| {
                matches!(role, A11yRole::Table | A11yRole::Grid)
            }) else {
                return build_row_entry(tree, node, 0).children;
            };
            let index = tables.table(tree, table);
            index
                .row_of
                .get(&node)
                .and_then(|position| index.rows.get(*position))
                .map_or_else(
                    || build_row_entry(tree, node, 0).children,
                    |row| row.children.clone(),
                )
        }
        A11yRole::Tree => tables
            .tree(tree, node)
            .children
            .get(&node)
            .cloned()
            .unwrap_or_default(),
        A11yRole::TreeItem => {
            let root = tree_collection_root(tree, node).unwrap_or(node);
            tables
                .tree(tree, root)
                .children
                .get(&node)
                .cloned()
                .unwrap_or_else(|| logical_physical_children(tree, node))
        }
        _ => logical_physical_children(tree, node),
    }
}

fn logical_physical_children(tree: &NodeTree, node: NodeId) -> Vec<NodeId> {
    children_of(tree, node)
        .iter()
        .copied()
        .filter(|child| logical_a11y_parent(tree, *child) == Some(node))
        .collect()
}

fn hidden_by_skipped_ancestors(tree: &NodeTree, node: NodeId, a11y_parent: Option<NodeId>) -> bool {
    let mut cursor = tree.get(node).ok().and_then(Node::parent);
    while let Some(current) = cursor {
        if Some(current) == a11y_parent {
            return false;
        }
        let Ok(target) = tree.get(current) else {
            return false;
        };
        let slot = tree.a11y_of(current);
        let over = slot.and_then(|slot| slot.over.as_ref());
        let patch = slot.map(|slot| &slot.patch);
        let hidden = patch
            .and_then(|patch| patch.hidden)
            .unwrap_or_else(|| over.is_some_and(|over| over.hidden));
        if hidden
            || target.style_patch().visible == Some(false)
            || matches!(target.style().inner().display, taffy::style::Display::None)
        {
            return true;
        }
        cursor = target.parent();
    }
    false
}

fn children_of(tree: &NodeTree, node: NodeId) -> &[NodeId] {
    tree.get(node).map_or(&[], |target| target.children())
}

fn hidden_outside_modal_scope(tree: &NodeTree, node: NodeId) -> bool {
    if node == tree.root() {
        return false;
    }
    let Some(scope) = crate::interaction::overlay::focus_scope_root(tree, tree.root()) else {
        return false;
    };
    !crate::interaction::overlay::in_modal_scope(tree, node, Some(scope))
}

pub(crate) fn mark_modal_scope_changed(tree: &mut NodeTree) {
    let mut ids = Vec::with_capacity(tree.len());
    tree.for_each_subtree(tree.root(), |node| ids.push(node));
    for node in ids {
        tree.mark_a11y(node);
    }
}

fn derive_table_semantics(
    tree: &NodeTree,
    node: NodeId,
    role: A11yRole,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
    tables: &mut CollectionSemanticsCache,
) -> TableSemantics {
    let mut semantics = TableSemantics {
        row_index: patch
            .and_then(|patch| patch.row_index)
            .or_else(|| over.and_then(|over| over.row_index)),
        column_index: patch
            .and_then(|patch| patch.column_index)
            .or_else(|| over.and_then(|over| over.column_index)),
        row_count: patch
            .and_then(|patch| patch.row_count)
            .or_else(|| over.and_then(|over| over.row_count)),
        column_count: patch
            .and_then(|patch| patch.column_count)
            .or_else(|| over.and_then(|over| over.column_count)),
    };
    match role {
        A11yRole::Table | A11yRole::Grid => {
            let index = tables.table(tree, node);
            if semantics.row_count.is_none() {
                semantics.row_count = index.row_count;
            }
            if semantics.column_count.is_none() {
                semantics.column_count = index.column_count;
            }
        }
        A11yRole::Row => {
            let Some(table) = nearest_ancestor_where(tree, node, |role| {
                matches!(role, A11yRole::Table | A11yRole::Grid)
            }) else {
                return semantics;
            };
            if semantics.row_index.is_none() || semantics.row_count.is_none() {
                let index = tables.table(tree, table);
                if semantics.row_index.is_none() {
                    semantics.row_index = index
                        .row_of
                        .get(&node)
                        .and_then(|position| index.rows.get(*position))
                        .map(|row| row.index);
                }
                if semantics.row_count.is_none() {
                    semantics.row_count = declared_row_count(tree, table).or(index.row_count);
                }
            }
        }
        A11yRole::Cell | A11yRole::ColumnHeader | A11yRole::RowHeader => {
            let Some(row) = nearest_ancestor_where(tree, node, |role| role == A11yRole::Row) else {
                if semantics.column_index.is_none() {
                    semantics.column_index = cell_sibling_index(tree, node);
                }
                return semantics;
            };
            if let Some(table) = nearest_ancestor_where(tree, row, |role| {
                matches!(role, A11yRole::Table | A11yRole::Grid)
            }) && (semantics.row_index.is_none()
                || semantics.column_index.is_none()
                || semantics.row_count.is_none()
                || semantics.column_count.is_none())
            {
                let index = tables.table(tree, table);
                let row_entry = index
                    .row_of
                    .get(&row)
                    .and_then(|position| index.rows.get(*position));
                if semantics.row_index.is_none() {
                    semantics.row_index = row_entry.map(|row| row.index);
                }
                if semantics.column_index.is_none() {
                    semantics.column_index =
                        row_entry.and_then(|row| row.column_of.get(&node).copied());
                }
                if semantics.row_count.is_none() {
                    semantics.row_count = declared_row_count(tree, table).or(index.row_count);
                }
                if semantics.column_count.is_none() {
                    semantics.column_count =
                        declared_column_count(tree, table).or(index.column_count);
                }
            }
            if semantics.column_index.is_none() {
                semantics.column_index = cell_sibling_index(tree, node);
            }
        }
        _ => {}
    }
    semantics
}

fn cell_sibling_index(tree: &NodeTree, node: NodeId) -> Option<usize> {
    let parent = tree.get(node).ok()?.parent()?;
    let mut index = 0usize;
    for child in children_of(tree, parent) {
        if *child == node {
            return Some(index);
        }
        if matches!(
            declared_role(tree, *child),
            Some(A11yRole::Cell | A11yRole::ColumnHeader | A11yRole::RowHeader)
        ) {
            index += 1;
        }
    }
    None
}

fn declared_row_count(tree: &NodeTree, table: NodeId) -> Option<usize> {
    let slot = tree.a11y_of(table)?;
    slot.patch
        .row_count
        .or_else(|| slot.over.as_ref().and_then(|over| over.row_count))
}

fn declared_column_count(tree: &NodeTree, table: NodeId) -> Option<usize> {
    let slot = tree.a11y_of(table)?;
    slot.patch
        .column_count
        .or_else(|| slot.over.as_ref().and_then(|over| over.column_count))
}

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

pub(crate) fn push_ancestors(tree: &NodeTree, from: NodeId, out: &mut SmallVec<[NodeId; 16]>) {
    let mut cursor = tree.get(from).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        out.push(current);
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
}

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
    use crate::{
        a11y::{A11yAction, A11yOverride, A11yRole, A11ySort, identity::A11yIds},
        math::Scale,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        text::TextStyle,
    };

    fn chain(tree: &mut NodeTree) -> (NodeId, NodeId, NodeId) {
        let root = tree.root();
        let parent = NodeBuilder::boxed().build(tree, root).expect("parent");
        let child = NodeBuilder::boxed().build(tree, parent).expect("child");
        (root, parent, child)
    }

    #[test]
    fn live_region_reaches_the_accesskit_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let price = NodeBuilder::boxed()
            .a11y(crate::a11y::A11yOverride {
                role: Some(crate::a11y::A11yRole::Label),
                label: Some(smol_str::SmolStr::new_static("BTC 42000")),
                live: Some(crate::a11y::A11yLive::Polite),
                ..crate::a11y::A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("price");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let node = super::node_a11y(
            &tree,
            price,
            crate::math::Scale::ONE,
            "",
            &mut ids,
            &mut diagnostics,
        )
        .expect("snapshot");
        assert_eq!(node.live, Some(crate::a11y::A11yLive::Polite));
        let ak = crate::a11y::adapter::to_accesskit(&node);
        assert_eq!(ak.live(), Some(accesskit::Live::Polite));
    }
    #[test]
    fn numeric_value_reaches_the_accesskit_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let slider = NodeBuilder::boxed()
            .a11y(crate::a11y::A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                label: Some(smol_str::SmolStr::new_static("volume")),
                numeric_value: Some(37.0),
                min_numeric_value: Some(0.0),
                max_numeric_value: Some(100.0),
                numeric_value_step: Some(1.0),
                numeric_value_jump: Some(10.0),
                ..crate::a11y::A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("slider");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let node = super::node_a11y(
            &tree,
            slider,
            crate::math::Scale::ONE,
            "",
            &mut ids,
            &mut diagnostics,
        )
        .expect("snapshot");
        assert_eq!(node.numeric_value, Some(37.0));
        assert_eq!(node.max_numeric_value, Some(100.0));
        let ak = crate::a11y::adapter::to_accesskit(&node);
        assert_eq!(ak.numeric_value(), Some(37.0));
        assert_eq!(ak.min_numeric_value(), Some(0.0));
        assert_eq!(ak.max_numeric_value(), Some(100.0));
        assert_eq!(ak.numeric_value_step(), Some(1.0));
        assert_eq!(ak.numeric_value_jump(), Some(10.0));
    }
    #[test]
    fn row_and_column_semantics_come_from_the_static_override() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let row = NodeBuilder::boxed()
            .a11y(crate::a11y::A11yOverride {
                role: Some(crate::a11y::A11yRole::Row),
                row_index: Some(3),
                row_count: Some(100),
                ..crate::a11y::A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("row");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let node = super::node_a11y(
            &tree,
            row,
            crate::math::Scale::ONE,
            "",
            &mut ids,
            &mut diagnostics,
        )
        .expect("snapshot");
        assert_eq!(node.row_index, Some(3));
        assert_eq!(node.row_count, Some(100));
        assert_eq!(node.column_index, None, "未声明列语义时不得凭空编造");
        let ak = crate::a11y::adapter::to_accesskit(&node);
        assert_eq!(ak.row_index(), Some(3));
        assert_eq!(ak.row_count(), Some(100));
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

    #[test]
    fn table_semantics_are_derived_from_structure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                label: Some(smol_str::SmolStr::new_static("行情")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");

        fn snapshot(tree: &NodeTree, node: NodeId) -> crate::a11y::A11yNode {
            let mut diagnostics = Vec::new();
            let mut ids = A11yIds::new();
            super::node_a11y(tree, node, Scale::ONE, "", &mut ids, &mut diagnostics)
                .expect("snapshot")
        }

        let mut rows = Vec::new();
        let mut cells = Vec::new();
        for index in 0..5 {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    ..A11yOverride::default()
                })
                .build(&mut tree, table)
                .expect("row");
            rows.push(row);
            for column in 0..3 {
                let role = if index == 0 {
                    A11yRole::ColumnHeader
                } else {
                    A11yRole::Cell
                };
                let cell = NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        role: Some(role),
                        ..A11yOverride::default()
                    })
                    .build(&mut tree, row)
                    .expect("cell");
                cells.push((cell, index, column));
            }
        }

        let table_snapshot = snapshot(&tree, table);
        assert_eq!(table_snapshot.row_count, Some(5), "行数从 Row 子节点数推导");
        assert_eq!(
            table_snapshot.column_count,
            Some(3),
            "列数从行里的 Cell 数最大值推导"
        );

        for (cell, row_index, column_index) in cells {
            let cell_snapshot = snapshot(&tree, cell);
            assert_eq!(cell_snapshot.row_index, Some(row_index), "行号自动推导");
            assert_eq!(
                cell_snapshot.column_index,
                Some(column_index),
                "列号自动推导（含表头行）"
            );
            assert_eq!(cell_snapshot.row_count, Some(5));
            assert_eq!(cell_snapshot.column_count, Some(3));
        }
        assert_eq!(snapshot(&tree, rows[3]).row_index, Some(3));
        let header = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ColumnHeader),
                label: Some(smol_str::SmolStr::new_static("价格")),
                sort: Some(A11ySort::Descending),
                ..A11yOverride::default()
            })
            .build(&mut tree, rows[0])
            .expect("header");
        let header_snapshot = snapshot(&tree, header);
        let ak = crate::a11y::adapter::to_accesskit(&header_snapshot);
        assert_eq!(ak.role(), accesskit::Role::ColumnHeader);
        assert_eq!(
            ak.sort_direction(),
            Some(accesskit::SortDirection::Descending)
        );
        assert_eq!(snapshot(&tree, table).column_count, Some(4));
    }

    #[test]
    fn a_large_table_builds_the_semantics_index_once() {
        const ROWS: usize = 5000;
        const COLS: usize = 8;
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let mut last_row = None;
        let mut last_cell = None;
        for row_index in 0..ROWS {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    ..A11yOverride::default()
                })
                .build(&mut tree, table)
                .expect("row");
            for column_index in 0..COLS {
                let cell = NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        role: Some(A11yRole::Cell),
                        ..A11yOverride::default()
                    })
                    .build(&mut tree, row)
                    .expect("cell");
                if row_index + 1 == ROWS && column_index + 1 == COLS {
                    last_cell = Some(cell);
                }
            }
            if row_index + 1 == ROWS {
                last_row = Some(row);
            }
        }

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let mut diagnostics = Vec::new();
        let table_node = super::node_a11y_with_cache(
            &tree,
            table,
            Scale::ONE,
            "",
            &mut ids,
            &mut tables,
            &mut diagnostics,
        )
        .expect("table snapshot");
        assert_eq!(table_node.row_count, Some(ROWS));
        assert_eq!(table_node.column_count, Some(COLS));
        assert_eq!(tables.builds(), 1, "表容器必须只建一次索引");

        let row_node = super::node_a11y_with_cache(
            &tree,
            last_row.expect("last row"),
            Scale::ONE,
            "",
            &mut ids,
            &mut tables,
            &mut diagnostics,
        )
        .expect("row snapshot");
        assert_eq!(row_node.row_index, Some(ROWS - 1));
        assert_eq!(row_node.row_count, Some(ROWS));

        let cell_node = super::node_a11y_with_cache(
            &tree,
            last_cell.expect("last cell"),
            Scale::ONE,
            "",
            &mut ids,
            &mut tables,
            &mut diagnostics,
        )
        .expect("cell snapshot");
        assert_eq!(cell_node.row_index, Some(ROWS - 1));
        assert_eq!(cell_node.column_index, Some(COLS - 1));
        assert_eq!(cell_node.row_count, Some(ROWS));
        assert_eq!(cell_node.column_count, Some(COLS));
        assert_eq!(
            tables.builds(),
            1,
            "同一笔交付内所有行列查询必须复用同一个索引"
        );
    }

    #[test]
    fn declared_table_semantics_share_one_structure_scan() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                row_count: Some(1),
                column_count: Some(1),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                row_index: Some(0),
                row_count: Some(1),
                ..A11yOverride::default()
            })
            .build(&mut tree, table)
            .expect("row");
        let cell = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Cell),
                row_index: Some(0),
                column_index: Some(0),
                row_count: Some(1),
                column_count: Some(1),
                ..A11yOverride::default()
            })
            .build(&mut tree, row)
            .expect("cell");

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let mut diagnostics = Vec::new();
        for (node, expected) in [
            (table, (Some(1), None, None)),
            (row, (Some(1), Some(0), None)),
            (cell, (Some(1), Some(0), Some(0))),
        ] {
            let record = super::node_a11y_with_cache(
                &tree,
                node,
                Scale::ONE,
                "",
                &mut ids,
                &mut tables,
                &mut diagnostics,
            )
            .expect("snapshot");
            assert_eq!(
                (record.row_count, record.row_index, record.column_index),
                expected,
                "node {node:?}"
            );
        }
        assert_eq!(
            tables.builds(),
            1,
            "表 / 行 / 单元格共享同一份集合索引（每笔一次，不按节点重扫）"
        );
    }

    #[test]
    fn a_cell_directly_under_a_table_keeps_its_sibling_column_index() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let cell = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Cell),
                ..A11yOverride::default()
            })
            .build(&mut tree, table)
            .expect("cell");
        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let mut diagnostics = Vec::new();
        let record = super::node_a11y_with_cache(
            &tree,
            cell,
            Scale::ONE,
            "",
            &mut ids,
            &mut tables,
            &mut diagnostics,
        )
        .expect("cell snapshot");
        assert_eq!(record.column_index, Some(0));
    }

    #[test]
    fn text_runs_expose_grapheme_lengths_and_caret() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(smol_str::SmolStr::new_static("数量")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");
        let text = NodeBuilder::text("价🌍ab", TextStyle::new(12.0))
            .build(&mut tree, input)
            .expect("text");
        tree.set_a11y_patch(
            text,
            crate::a11y::A11yPatch {
                selection: Some(crate::a11y::A11yTextSelection {
                    anchor: 7,
                    focus: 7,
                }),
                ..crate::a11y::A11yPatch::default()
            },
        );
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let snapshot = super::node_a11y(&tree, text, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert_eq!(snapshot.role, A11yRole::TextRun, "输入框内的文本是 TextRun");
        assert_eq!(
            snapshot.character_lengths,
            vec![3, 4, 1, 1],
            "grapheme 字节长度（CJK 3 / emoji 4）"
        );
        assert_eq!(
            snapshot.text_selection,
            Some(crate::a11y::A11yTextSelection {
                anchor: 2,
                focus: 2
            }),
            "字节偏移 7 → grapheme 下标 2"
        );
        let ak = crate::a11y::adapter::to_accesskit(&snapshot);
        assert_eq!(ak.character_lengths(), &[3, 4, 1, 1]);
        assert_eq!(ak.value(), Some("价🌍ab"));
        let selection = ak.text_selection().expect("text_selection");
        assert_eq!(selection.focus.character_index, 2);
        assert_eq!(
            selection.focus.node,
            accesskit::NodeId(snapshot.id.get()),
            "TextPosition.node 必须是 TextRun 自己"
        );
    }

    #[test]
    fn empty_text_has_no_grapheme_lengths() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let text = NodeBuilder::text("", TextStyle::new(12.0))
            .build(&mut tree, root)
            .expect("text");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let snapshot = super::node_a11y(&tree, text, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert!(snapshot.character_lengths.is_empty());
        assert!(snapshot.text_selection.is_none());
    }

    #[test]
    fn custom_actions_are_registered_on_the_accesskit_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("BTC/USDT")),
                actions: Some(vec![
                    crate::a11y::A11yAction::Click,
                    crate::a11y::A11yAction::Custom(7),
                ]),
                custom_actions: vec![crate::a11y::A11yCustomAction {
                    id: 7,
                    label: smol_str::SmolStr::new_static("加入自选"),
                }],
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("row");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let snapshot = super::node_a11y(&tree, row, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let ak = crate::a11y::adapter::to_accesskit(&snapshot);
        assert!(ak.supports_action(accesskit::Action::CustomAction));
        assert_eq!(ak.custom_actions().len(), 1);
        assert_eq!(ak.custom_actions()[0].id, 7);
        assert_eq!(ak.custom_actions()[0].description, "加入自选");
    }

    #[test]
    fn unregistered_custom_actions_are_diagnosed() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("x")),
                actions: Some(vec![crate::a11y::A11yAction::Custom(3)]),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("node");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let _ = super::node_a11y(&tree, node, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert!(
            diagnostics.iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::UnregisteredCustomAction { id: 3, .. }
            )),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn snapshot_ids_are_stable_across_calls() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let first = NodeBuilder::boxed().build(&mut tree, root).expect("a");
        let second = NodeBuilder::boxed().build(&mut tree, root).expect("b");
        let mut ids = A11yIds::new();
        let mut diagnostics = Vec::new();
        let a = super::node_a11y(&tree, first, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("a snapshot");
        let b = super::node_a11y(&tree, second, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("b snapshot");
        let a2 = super::node_a11y(&tree, first, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("a again");
        assert_eq!(a.id, a2.id, "同一节点跨帧 id 必须不变");
        assert_ne!(a.id, b.id, "不同节点不得共享 id");
        assert_eq!(a.parent, ids.stable_of(root), "父 id 来自同一分配器");
        assert_eq!(ids.sizes().0, 3, "root + first + second 各一个引擎绑定");
    }

    #[test]
    fn text_selection_only_lives_on_text_run() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(smol_str::SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");
        let text = NodeBuilder::text("价🌍ab", TextStyle::new(12.0))
            .build(&mut tree, input)
            .expect("text run");
        let selection = crate::a11y::A11yTextSelection {
            anchor: 0,
            focus: 3,
        };
        tree.set_a11y_patch(
            input,
            crate::a11y::A11yPatch {
                selection: Some(selection),
                ..crate::a11y::A11yPatch::default()
            },
        );
        tree.set_a11y_patch(
            text,
            crate::a11y::A11yPatch {
                selection: Some(selection),
                ..crate::a11y::A11yPatch::default()
            },
        );

        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let input_snapshot =
            super::node_a11y(&tree, input, Scale::ONE, "", &mut ids, &mut diagnostics)
                .expect("input snapshot");
        assert!(
            input_snapshot.character_lengths.is_empty(),
            "输入框不携带字符语义（由子 TextRun 承担）"
        );
        assert_eq!(
            input_snapshot.text_selection, None,
            "打在输入框上的选区必须被忽略（consumer 只认 TextPosition.node = TextRun）"
        );
        assert!(input_snapshot.actions.contains(&A11yAction::SetValue));
        assert!(
            !input_snapshot
                .actions
                .contains(&A11yAction::SetTextSelection),
            "输入框默认只宣告 SetValue，选区能力挂在 TextRun 上"
        );

        let run_snapshot =
            super::node_a11y(&tree, text, Scale::ONE, "", &mut ids, &mut diagnostics)
                .expect("text run snapshot");
        assert_eq!(run_snapshot.role, A11yRole::TextRun);
        assert_eq!(run_snapshot.character_lengths, vec![3, 4, 1, 1]);
        assert!(
            run_snapshot.character_positions.is_empty() && run_snapshot.character_widths.is_empty(),
            "未 shape 的文本不得输出 character_positions/widths"
        );
        assert_eq!(
            run_snapshot.text_selection,
            Some(crate::a11y::A11yTextSelection {
                anchor: 0,
                focus: 1
            }),
            "字节 3（emoji 之后）= grapheme 1"
        );
        assert!(
            run_snapshot.actions.contains(&A11yAction::SetTextSelection),
            "TextRun 默认宣告 SetTextSelection"
        );
    }
    fn snapshot_with(
        tree: &NodeTree,
        node: NodeId,
        ids: &mut A11yIds,
        tables: &mut super::CollectionSemanticsCache,
    ) -> crate::a11y::A11yNode {
        let mut diagnostics = Vec::new();
        super::node_a11y_with_cache(tree, node, Scale::ONE, "", ids, tables, &mut diagnostics)
            .expect("snapshot")
    }

    fn declare_overlay(
        tree: &mut NodeTree,
        node: NodeId,
        trap_focus: bool,
        opener: Option<NodeId>,
    ) {
        tree.ensure_interaction(node).expect("interaction").overlay =
            Some(crate::interaction::OverlaySlot {
                dismiss: crate::interaction::OverlayDismiss::Explicit,
                placement: crate::interaction::OverlayPlacement::Point(crate::math::Point::ZERO),
                trap_focus,
                opener,
                restore_focus: false,
            });
    }

    fn assert_single_parent_per_node(
        tree: &NodeTree,
        ids: &mut A11yIds,
        tables: &mut super::CollectionSemanticsCache,
    ) {
        let mut subtree = SmallVec::new();
        super::collect_subtree(tree, tree.root(), &mut subtree);
        let records: Vec<crate::a11y::A11yNode> = subtree
            .iter()
            .map(|node| snapshot_with(tree, *node, ids, tables))
            .collect();
        let by_id: ahash::HashMap<crate::a11y::A11yNodeId, &crate::a11y::A11yNode> =
            records.iter().map(|record| (record.id, record)).collect();
        assert_eq!(by_id.len(), records.len(), "a11y id 必须唯一");
        let mut parent_of: ahash::HashMap<crate::a11y::A11yNodeId, crate::a11y::A11yNodeId> =
            ahash::HashMap::default();
        for record in &records {
            for child in &record.children {
                let Some(child_record) = by_id.get(child) else {
                    panic!(
                        "节点 {:?} 的 children 里有未知子节点 {child:?}（不可达 / 孤儿）",
                        record.engine
                    );
                };
                if let Some(previous) = parent_of.insert(*child, record.id) {
                    assert_eq!(
                        previous, record.id,
                        "节点 {:?} 同时挂在 {previous:?} 与 {:?} 两个父下",
                        child_record.engine, record.id
                    );
                }
            }
        }
        for record in &records {
            let Some(parent) = record.parent else {
                continue;
            };
            let Some(parent_record) = by_id.get(&parent) else {
                continue;
            };
            assert!(
                parent_record.children.contains(&record.id),
                "节点 {:?} 的父 {parent:?} 没有把它列进 children（孤儿）",
                record.engine
            );
        }
    }

    #[test]
    fn a_virtualized_table_hangs_rows_under_the_table() {
        const FIRST: usize = 10;
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                label: Some(smol_str::SmolStr::new_static("行情")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let header = NodeBuilder::boxed()
            .build(&mut tree, table)
            .expect("header");
        let header_row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .build(&mut tree, header)
            .expect("header row");
        let _header_cell = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ColumnHeader),
                label: Some(smol_str::SmolStr::new_static("价格")),
                ..A11yOverride::default()
            })
            .build(&mut tree, header_row)
            .expect("header cell");
        let list = NodeBuilder::boxed().build(&mut tree, table).expect("list");
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, list)
            .expect("viewport");
        let mut rows = Vec::with_capacity(3);
        let mut cells = Vec::with_capacity(3);
        for _ in 0..3 {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    ..A11yOverride::default()
                })
                .build(&mut tree, viewport)
                .expect("row");
            let column = NodeBuilder::boxed().build(&mut tree, row).expect("column");
            let cells_row = NodeBuilder::boxed()
                .build(&mut tree, column)
                .expect("cells row");
            let cell = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Cell),
                    label: Some(smol_str::SmolStr::new_static("42")),
                    ..A11yOverride::default()
                })
                .build(&mut tree, cells_row)
                .expect("cell");
            rows.push(row);
            cells.push(cell);
        }
        let mut slot = crate::interaction::VirtualSlot::new(20.0, 0);
        slot.count = 100;
        slot.mounted = rows
            .iter()
            .enumerate()
            .map(|(offset, row)| (FIRST + offset, *row))
            .collect();
        tree.ensure_interaction(viewport)
            .expect("viewport interaction")
            .virtual_list = Some(slot);

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let table_record = snapshot_with(&tree, table, &mut ids, &mut tables);
        fn stable(node: NodeId, ids: &A11yIds) -> crate::a11y::A11yNodeId {
            ids.stable_of(node).expect("stable id")
        }

        assert_eq!(
            table_record.children,
            vec![
                stable(header, &ids),
                stable(header_row, &ids),
                stable(list, &ids),
                stable(rows[0], &ids),
                stable(rows[1], &ids),
                stable(rows[2], &ids),
            ],
            "表的逻辑子节点必须是文档序里的行 + 未被领养走的包裹层"
        );
        assert_eq!(table_record.role, A11yRole::Table);
        assert_eq!(
            table_record.row_count,
            Some(101),
            "逻辑总行数 = 表头 1 行 + 行窗口的 100 个条目（含未挂载）"
        );
        assert_eq!(table_record.column_count, Some(1));

        let header_row_record = snapshot_with(&tree, header_row, &mut ids, &mut tables);
        assert_eq!(
            header_row_record.parent,
            Some(table_record.id),
            "表头行的 a11y 父也必须是 Table"
        );
        assert_eq!(header_row_record.row_index, Some(0), "表头行 = 第 0 行");
        assert_eq!(header_row_record.row_count, Some(101));
        for (offset, row) in rows.iter().enumerate() {
            let record = snapshot_with(&tree, *row, &mut ids, &mut tables);
            assert_eq!(
                record.parent,
                Some(table_record.id),
                "虚拟化表格的行必须挂到 Table 下（List 行窗口不是 a11y 父）"
            );
            assert_eq!(
                record.row_index,
                Some(1 + FIRST + offset),
                "行号 = 窗口基址（表头 1 行）+ 条目的逻辑下标"
            );
            assert_eq!(record.row_count, Some(101), "总数含未挂载的条目");
            let accesskit_node = crate::a11y::adapter::to_accesskit(&record);
            assert_eq!(accesskit_node.role(), accesskit::Role::Row);
            assert_eq!(accesskit_node.row_index(), Some(1 + FIRST + offset));
            assert_eq!(accesskit_node.row_count(), Some(101));
        }

        let viewport_record = snapshot_with(&tree, viewport, &mut ids, &mut tables);
        assert_eq!(
            viewport_record.role,
            A11yRole::GenericContainer,
            "集合行窗口不得再自动声明 `List`（E15）"
        );
        assert!(
            viewport_record.children.is_empty(),
            "行被领养到 Table 后，行窗口不得再列出行"
        );

        for (offset, cell) in cells.iter().enumerate() {
            let record = snapshot_with(&tree, *cell, &mut ids, &mut tables);
            assert_eq!(record.parent, Some(stable(rows[offset], &ids)));
            assert_eq!(record.row_index, Some(1 + FIRST + offset));
            assert_eq!(record.column_index, Some(0));
            assert_eq!(record.column_count, Some(1));
            assert_eq!(record.row_count, Some(101));
        }
    }

    #[test]
    fn a_suppressed_row_window_lifts_its_scroll_semantics_to_the_table() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                label: Some(smol_str::SmolStr::new_static("行情")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let list = NodeBuilder::boxed().build(&mut tree, table).expect("list");
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, list)
            .expect("viewport");
        let mut rows = Vec::with_capacity(2);
        for _ in 0..2 {
            rows.push(
                NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        role: Some(A11yRole::Row),
                        ..A11yOverride::default()
                    })
                    .build(&mut tree, viewport)
                    .expect("row"),
            );
        }
        declare_virtual_window(&mut tree, viewport, 100, &[(0, rows[0]), (1, rows[1])]);
        tree.ensure_interaction(viewport)
            .expect("viewport interaction")
            .scroll = Some(crate::interaction::ScrollSlot {
            offset: crate::math::Point::new(0.0, 40.0),
            content: crate::math::Size::new(0.0, 2000.0),
            axis: crate::interaction::ScrollAxis::Vertical,
            line_height: 20.0,
        });

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let table_record = snapshot_with(&tree, table, &mut ids, &mut tables);
        let viewport_record = snapshot_with(&tree, viewport, &mut ids, &mut tables);
        assert_eq!(
            viewport_record.role,
            A11yRole::GenericContainer,
            "被抑制的窗口仍是平台透明层（它的属性由集合根接管）"
        );

        let scroll = table_record
            .scroll
            .expect("集合根必须接管窗口的 scroll（E9 R2 后半）");
        assert_eq!(scroll.y, 40.0);
        assert_eq!(scroll.y_max, 2000.0);
        assert!(scroll.vertical);
        assert!(
            table_record.clips_children,
            "滚动容器的裁剪感知必须随语义上提"
        );
        assert_eq!(table_record.size_of_set, Some(100));
        for action in [
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
            A11yAction::ScrollIntoView,
            A11yAction::SetScrollOffset,
        ] {
            assert!(
                table_record.actions.contains(&action),
                "集合根必须宣告 {action:?}（上提后的滚动动作）"
            );
        }
        assert_eq!(
            super::a11y_scroll_target(&tree, table),
            Some(viewport),
            "集合根的滚动目标 = 上提的窗口（不是 Table 自身、更不是物理滚动祖先）"
        );
        assert_eq!(
            super::a11y_scroll_target(&tree, rows[0]),
            Some(viewport),
            "行上的滚动动作仍落到窗口"
        );

        let ak = crate::a11y::adapter::to_accesskit(&table_record);
        assert_eq!(ak.scroll_y(), Some(40.0));
        assert_eq!(ak.scroll_y_max(), Some(2000.0));
        assert!(ak.clips_children());
        assert!(ak.supports_action(accesskit::Action::ScrollDown));
    }

    #[test]
    fn tree_items_hang_under_the_tree_and_carry_their_state() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let tree_node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Tree),
                label: Some(smol_str::SmolStr::new_static("持仓")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("tree");
        let panel = NodeBuilder::boxed()
            .build(&mut tree, tree_node)
            .expect("panel");
        let item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TreeItem),
                label: Some(smol_str::SmolStr::new_static("BTC")),
                ..A11yOverride::default()
            })
            .build(&mut tree, panel)
            .expect("item");
        let inner = NodeBuilder::boxed().build(&mut tree, item).expect("inner");
        let child_item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TreeItem),
                label: Some(smol_str::SmolStr::new_static("现货")),
                ..A11yOverride::default()
            })
            .build(&mut tree, inner)
            .expect("child item");
        let sibling = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TreeItem),
                label: Some(smol_str::SmolStr::new_static("ETH")),
                ..A11yOverride::default()
            })
            .build(&mut tree, panel)
            .expect("sibling");
        tree.set_a11y_patch(
            item,
            crate::a11y::A11yPatch {
                expanded: Some(true),
                selected: Some(true),
                ..crate::a11y::A11yPatch::default()
            },
        );
        tree.set_a11y_patch(
            sibling,
            crate::a11y::A11yPatch {
                expanded: Some(false),
                disabled: Some(true),
                ..crate::a11y::A11yPatch::default()
            },
        );

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        fn stable(node: NodeId, ids: &A11yIds) -> crate::a11y::A11yNodeId {
            ids.stable_of(node).expect("stable id")
        }
        let tree_record = snapshot_with(&tree, tree_node, &mut ids, &mut tables);
        assert_eq!(tree_record.role, A11yRole::Tree);
        assert_eq!(
            tree_record.children,
            vec![
                stable(panel, &ids),
                stable(item, &ids),
                stable(sibling, &ids)
            ],
            "树项必须挂到 Tree 下（包裹层 panel 只是普通子节点）"
        );
        let item_record = snapshot_with(&tree, item, &mut ids, &mut tables);
        assert_eq!(item_record.parent, Some(tree_record.id), "树项的父是 Tree");
        assert_eq!(item_record.expanded, Some(true));
        assert_eq!(item_record.selected, Some(true));
        let child_record = snapshot_with(&tree, child_item, &mut ids, &mut tables);
        assert_eq!(
            child_record.parent,
            Some(item_record.id),
            "嵌套树项的父是最近的 TreeItem"
        );
        let sibling_record = snapshot_with(&tree, sibling, &mut ids, &mut tables);
        assert_eq!(sibling_record.parent, Some(tree_record.id));
        assert_eq!(sibling_record.expanded, Some(false));
        assert!(sibling_record.disabled, "禁用状态必须进 a11y 节点");
        let accesskit_item = crate::a11y::adapter::to_accesskit(&item_record);
        assert_eq!(accesskit_item.role(), accesskit::Role::TreeItem);
        assert_eq!(accesskit_item.is_expanded(), Some(true));
        assert_eq!(accesskit_item.is_selected(), Some(true));
        assert_eq!(
            crate::a11y::adapter::to_accesskit(&tree_record).role(),
            accesskit::Role::Tree
        );
    }

    #[test]
    fn a_hidden_row_window_hides_the_adopted_rows() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let window = NodeBuilder::boxed()
            .build(&mut tree, table)
            .expect("window");
        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .build(&mut tree, window)
            .expect("row");

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        assert!(
            !snapshot_with(&tree, row, &mut ids, &mut tables).hidden,
            "前置条件：未隐藏时行可见"
        );
        tree.set_a11y_patch(
            window,
            crate::a11y::A11yPatch {
                hidden: Some(true),
                ..crate::a11y::A11yPatch::default()
            },
        );
        assert!(
            snapshot_with(&tree, row, &mut ids, &mut tables).hidden,
            "被领养跳过的物理祖先隐藏时，行必须继承隐藏语义"
        );
    }

    #[test]
    fn a_non_row_virtual_window_inside_a_table_is_not_adopted_as_rows() {
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
        let mut items = Vec::with_capacity(3);
        for _ in 0..3 {
            items.push(
                NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        role: Some(A11yRole::ListBoxOption),
                        ..A11yOverride::default()
                    })
                    .build(&mut tree, viewport)
                    .expect("item"),
            );
        }
        let mut slot = crate::interaction::VirtualSlot::new(20.0, 0);
        slot.count = 30;
        slot.mounted = items
            .iter()
            .enumerate()
            .map(|(index, item)| (index, *item))
            .collect();
        tree.ensure_interaction(viewport)
            .expect("viewport interaction")
            .virtual_list = Some(slot);

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let table_record = snapshot_with(&tree, table, &mut ids, &mut tables);
        assert_eq!(
            table_record.row_count, None,
            "非行窗口的 slot.count 不得计入 row_count（旧实现虚增成 30）"
        );
        assert_eq!(table_record.column_count, None);
        assert_eq!(
            table_record.children,
            vec![ids.stable_of(list).expect("list id")],
            "非行窗口的挂载项不得被领养到 Table 下"
        );
        let viewport_record = snapshot_with(&tree, viewport, &mut ids, &mut tables);
        assert_eq!(
            viewport_record.children.len(),
            items.len(),
            "窗口仍按物理父子列出自条目"
        );
        for item in &items {
            let record = snapshot_with(&tree, *item, &mut ids, &mut tables);
            assert_eq!(
                record.parent,
                Some(viewport_record.id),
                "挂载项的 a11y 父必须还是物理父（行窗口），不能同时挂到 Table"
            );
        }
        assert_single_parent_per_node(&tree, &mut ids, &mut tables);
    }

    #[test]
    fn a_row_without_a_table_keeps_a_self_consistent_tree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list = NodeBuilder::boxed().build(&mut tree, root).expect("list");
        let wrapper = NodeBuilder::boxed()
            .build(&mut tree, list)
            .expect("wrapper");
        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .build(&mut tree, wrapper)
            .expect("row");
        let cells_row = NodeBuilder::boxed()
            .build(&mut tree, row)
            .expect("cells row");
        let cell = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Cell),
                ..A11yOverride::default()
            })
            .build(&mut tree, cells_row)
            .expect("cell");

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let row_record = snapshot_with(&tree, row, &mut ids, &mut tables);
        let cell_record = snapshot_with(&tree, cell, &mut ids, &mut tables);
        assert_eq!(
            row_record.parent,
            Some(ids.stable_of(wrapper).expect("wrapper id")),
            "无 Table/Grid 的 Row 不得被误领养（父 = 物理父）"
        );
        assert_eq!(
            cell_record.parent,
            Some(row_record.id),
            "Cell 的 a11y 父是最近的 Row（没有 Table 也一样）"
        );
        assert_eq!(
            row_record.children,
            vec![
                ids.stable_of(cells_row).expect("cells row id"),
                cell_record.id
            ],
            "Row 的 children 必须列出被领养的 Cell（否则 Cell 无父可达）"
        );
        assert_eq!(row_record.row_index, None, "没有表格就没有行号可言");
        assert_single_parent_per_node(&tree, &mut ids, &mut tables);
    }

    #[test]
    fn a_nested_tree_inside_a_tree_item_indexes_its_own_items() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Tree),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("outer tree");
        let outer_item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TreeItem),
                ..A11yOverride::default()
            })
            .build(&mut tree, outer)
            .expect("outer item");
        let inner = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Tree),
                ..A11yOverride::default()
            })
            .build(&mut tree, outer_item)
            .expect("inner tree");
        let inner_item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TreeItem),
                ..A11yOverride::default()
            })
            .build(&mut tree, inner)
            .expect("inner item");
        let wrapper = NodeBuilder::boxed()
            .build(&mut tree, inner_item)
            .expect("wrapper");
        let nested_item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TreeItem),
                ..A11yOverride::default()
            })
            .build(&mut tree, wrapper)
            .expect("nested item");

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let outer_record = snapshot_with(&tree, outer, &mut ids, &mut tables);
        let outer_item_record = snapshot_with(&tree, outer_item, &mut ids, &mut tables);
        let inner_record = snapshot_with(&tree, inner, &mut ids, &mut tables);
        let inner_item_record = snapshot_with(&tree, inner_item, &mut ids, &mut tables);
        let nested_record = snapshot_with(&tree, nested_item, &mut ids, &mut tables);
        assert_eq!(
            outer_record.children,
            vec![outer_item_record.id],
            "外层 Tree 只列自己的树项"
        );
        assert_eq!(
            outer_item_record.children,
            vec![inner_record.id],
            "嵌套 Tree 的 a11y 父是最近的 TreeItem（物理父）"
        );
        assert_eq!(
            inner_record.children,
            vec![inner_item_record.id],
            "嵌套 Tree 有自己的索引：只列自己的树项"
        );
        assert_eq!(
            inner_item_record.children,
            vec![
                ids.stable_of(wrapper).expect("wrapper id"),
                nested_record.id
            ],
            "内层树项的 children 必须完整（包裹层 + 被领养的嵌套树项）"
        );
        assert_eq!(nested_record.parent, Some(inner_item_record.id));
        assert_single_parent_per_node(&tree, &mut ids, &mut tables);
    }

    #[test]
    fn nearest_collection_ancestor_finds_the_closest_declared_collection() {
        fn collection(tree: &mut NodeTree, parent: NodeId, role: A11yRole) -> NodeId {
            NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(role),
                    ..A11yOverride::default()
                })
                .build(tree, parent)
                .expect("collection")
        }

        let mut tree = NodeTree::new();
        let root = tree.root();
        let plain = NodeBuilder::boxed().build(&mut tree, root).expect("plain");
        let leaf = NodeBuilder::boxed().build(&mut tree, plain).expect("leaf");
        assert_eq!(
            super::nearest_collection_ancestor(&tree, leaf),
            None,
            "没有集合根 ⇒ None（裸虚拟列表维持既有行为）"
        );

        let table = collection(&mut tree, plain, A11yRole::Table);
        let wrapper = NodeBuilder::boxed()
            .build(&mut tree, table)
            .expect("wrapper");
        let inner = NodeBuilder::boxed()
            .build(&mut tree, wrapper)
            .expect("inner");
        assert_eq!(
            super::nearest_collection_ancestor(&tree, inner),
            Some(table),
            "跳过布局包裹层，返回最近的 Table"
        );

        let grid = collection(&mut tree, inner, A11yRole::Grid);
        let cell = NodeBuilder::boxed().build(&mut tree, grid).expect("cell");
        assert_eq!(
            super::nearest_collection_ancestor(&tree, cell),
            Some(grid),
            "嵌套集合：最近的 Grid 胜出"
        );

        let tree_root = collection(&mut tree, root, A11yRole::Tree);
        let item = NodeBuilder::boxed()
            .build(&mut tree, tree_root)
            .expect("item");
        assert_eq!(
            super::nearest_collection_ancestor(&tree, item),
            Some(tree_root),
            "Tree 同样是集合根"
        );
    }

    #[test]
    fn an_overlay_wrapper_is_a_container_unless_it_declares_a_role() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let plain = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("overlay");
        declare_overlay(&mut tree, plain, false, None);
        let menu = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Menu),
                label: Some(smol_str::SmolStr::new_static("交易菜单")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("menu");
        declare_overlay(&mut tree, menu, false, None);
        let dialog = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Dialog),
                label: Some(smol_str::SmolStr::new_static("确认下单")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("dialog");
        declare_overlay(&mut tree, dialog, false, None);
        let alert = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::AlertDialog),
                label: Some(smol_str::SmolStr::new_static("风险提示")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("alert");
        declare_overlay(&mut tree, alert, false, None);

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let plain_record = snapshot_with(&tree, plain, &mut ids, &mut tables);
        assert_eq!(
            plain_record.role,
            A11yRole::GenericContainer,
            "没有显式声明的浮层包装必须按容器处理（不得猜成 Dialog）"
        );
        assert!(
            plain_record.actions.is_empty(),
            "浮层包装不得自动宣告 Expand/Collapse：{:?}",
            plain_record.actions
        );
        assert!(!plain_record.modal, "非模态浮层的包装不是 modal");

        let menu_record = snapshot_with(&tree, menu, &mut ids, &mut tables);
        assert_eq!(menu_record.role, A11yRole::Menu, "显式 Menu 角色必须生效");
        assert_eq!(menu_record.label_text(), Some("交易菜单"));
        assert!(menu_record.actions.is_empty(), "Menu 默认不宣告动作");
        assert_eq!(
            crate::a11y::adapter::to_accesskit(&menu_record).role(),
            accesskit::Role::Menu
        );

        let dialog_record = snapshot_with(&tree, dialog, &mut ids, &mut tables);
        assert_eq!(dialog_record.role, A11yRole::Dialog);
        assert!(
            !dialog_record
                .actions
                .contains(&crate::a11y::A11yAction::Expand),
            "显式 Dialog 也不得默认宣告 Expand（aria-expanded 属于触发器）"
        );
        let alert_record = snapshot_with(&tree, alert, &mut ids, &mut tables);
        assert_eq!(alert_record.role, A11yRole::AlertDialog);
        assert_eq!(
            crate::a11y::adapter::to_accesskit(&alert_record).role(),
            accesskit::Role::AlertDialog
        );
    }

    #[test]
    fn a_modal_overlay_hides_the_background_from_a11y() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let background = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("下单")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("background");
        let stray_overlay = NodeBuilder::boxed().build(&mut tree, root).expect("popup");
        declare_overlay(&mut tree, stray_overlay, false, None);
        let stray_item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::MenuItem),
                label: Some(smol_str::SmolStr::new_static("复制")),
                ..A11yOverride::default()
            })
            .build(&mut tree, stray_overlay)
            .expect("popup item");
        let modal = NodeBuilder::boxed().build(&mut tree, root).expect("modal");
        declare_overlay(&mut tree, modal, true, None);
        let confirm = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("确认")),
                ..A11yOverride::default()
            })
            .build(&mut tree, modal)
            .expect("confirm");

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let root_record = snapshot_with(&tree, root, &mut ids, &mut tables);
        assert!(!root_record.hidden, "根是模态的祖先，必须保持可见");
        let background_record = snapshot_with(&tree, background, &mut ids, &mut tables);
        assert!(
            background_record.hidden,
            "模态作用域外的背景节点必须 aria-hidden"
        );
        assert!(
            crate::a11y::adapter::to_accesskit(&background_record).is_hidden(),
            "hidden 必须真的落进 accesskit::Node"
        );
        let stray_record = snapshot_with(&tree, stray_overlay, &mut ids, &mut tables);
        assert!(stray_record.hidden, "不属于该模态的浮层也必须隐藏");
        assert!(snapshot_with(&tree, stray_item, &mut ids, &mut tables).hidden);

        let modal_record = snapshot_with(&tree, modal, &mut ids, &mut tables);
        assert!(!modal_record.hidden, "模态包装本身必须可见");
        assert_eq!(
            modal_record.role,
            A11yRole::GenericContainer,
            "未声明的包装仍是容器（模态由 trap_focus 表达，不靠 Dialog 角色）"
        );
        assert!(modal_record.modal, "a11y modal 跟 trap_focus 走");
        let modal_ak = crate::a11y::adapter::to_accesskit(&modal_record);
        assert!(modal_ak.is_modal(), "accesskit::Node 必须带 modal 状态");
        assert!(!snapshot_with(&tree, confirm, &mut ids, &mut tables).hidden);

        tree.ensure_interaction(modal).expect("interaction").overlay = None;
        let mut fresh_ids = A11yIds::new();
        let mut fresh_tables = super::CollectionSemanticsCache::default();
        assert!(
            !snapshot_with(&tree, background, &mut fresh_ids, &mut fresh_tables).hidden,
            "没有模态时背景不得隐藏"
        );
    }

    #[test]
    fn a_popup_opened_inside_the_modal_stays_visible_to_a11y() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let modal = NodeBuilder::boxed().build(&mut tree, root).expect("modal");
        declare_overlay(&mut tree, modal, true, None);
        let trigger = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("选择数量")),
                ..A11yOverride::default()
            })
            .build(&mut tree, modal)
            .expect("trigger");
        let menu = NodeBuilder::boxed().build(&mut tree, root).expect("menu");
        declare_overlay(&mut tree, menu, false, Some(trigger));
        let item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::MenuItem),
                label: Some(smol_str::SmolStr::new_static("全部")),
                ..A11yOverride::default()
            })
            .build(&mut tree, menu)
            .expect("item");

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        assert!(
            !snapshot_with(&tree, menu, &mut ids, &mut tables).hidden,
            "opener 在模态内的弹出层属于作用域，不得隐藏"
        );
        assert!(
            !snapshot_with(&tree, item, &mut ids, &mut tables).hidden,
            "弹出层内容对 AT 必须可见（对话框里的下拉菜单）"
        );
    }

    fn declare_virtual_window(
        tree: &mut NodeTree,
        node: NodeId,
        count: usize,
        mounted: &[(usize, NodeId)],
    ) {
        let mut slot = crate::interaction::VirtualSlot::new(20.0, 0);
        slot.count = count;
        slot.mounted = mounted.to_vec();
        tree.ensure_interaction(node)
            .expect("interaction")
            .virtual_list = Some(slot);
        tree.note_virtual_container(node);
    }

    #[test]
    fn a_plain_virtual_list_keeps_the_list_role() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("viewport");
        let row = NodeBuilder::boxed()
            .build(&mut tree, viewport)
            .expect("row");
        declare_virtual_window(&mut tree, viewport, 10, &[(0, row)]);

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let record = snapshot_with(&tree, viewport, &mut ids, &mut tables);
        assert_eq!(
            record.role,
            A11yRole::List,
            "无集合祖先的虚拟列表容器必须仍是 List"
        );
        assert_eq!(
            crate::a11y::adapter::to_accesskit(&record).role(),
            accesskit::Role::List
        );
    }

    #[test]
    fn an_explicit_container_role_overrides_the_auto_list() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("viewport");
        let row = NodeBuilder::boxed()
            .build(&mut tree, viewport)
            .expect("row");
        declare_virtual_window(&mut tree, viewport, 3, &[(0, row)]);
        tree.set_a11y_patch(
            viewport,
            crate::a11y::A11yPatch {
                role: Some(A11yRole::ListBox),
                size_of_set: Some(3),
                ..crate::a11y::A11yPatch::default()
            },
        );

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let record = snapshot_with(&tree, viewport, &mut ids, &mut tables);
        assert_eq!(record.role, A11yRole::ListBox);
        let ak = crate::a11y::adapter::to_accesskit(&record);
        assert_eq!(ak.role(), accesskit::Role::ListBox);
        assert_eq!(ak.size_of_set(), Some(3));
    }

    #[test]
    fn a_tree_row_window_is_not_exposed_as_a_list() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let tree_node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Tree),
                label: Some(smol_str::SmolStr::new_static("持仓")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("tree");
        let wrapper = NodeBuilder::boxed()
            .build(&mut tree, tree_node)
            .expect("wrapper");
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, wrapper)
            .expect("viewport");
        let mut items = Vec::with_capacity(2);
        for _ in 0..2 {
            let item = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::TreeItem),
                    label: Some(smol_str::SmolStr::new_static("BTC")),
                    ..A11yOverride::default()
                })
                .build(&mut tree, viewport)
                .expect("tree item");
            items.push(item);
        }
        declare_virtual_window(&mut tree, viewport, 2, &[(0, items[0]), (1, items[1])]);

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let tree_record = snapshot_with(&tree, tree_node, &mut ids, &mut tables);
        let viewport_record = snapshot_with(&tree, viewport, &mut ids, &mut tables);
        assert_eq!(
            viewport_record.role,
            A11yRole::GenericContainer,
            "树的集合行窗口不得暴露成 List（AT 会读成「树里套列表」）"
        );
        assert!(
            viewport_record.children.is_empty(),
            "树项已被领养到 Tree 下，行窗口不得再列出它们"
        );
        for item in items {
            let record = snapshot_with(&tree, item, &mut ids, &mut tables);
            assert_eq!(record.role, A11yRole::TreeItem);
            assert_eq!(record.parent, Some(tree_record.id), "树项的 a11y 父是 Tree");
        }
    }

    #[test]
    fn an_empty_collection_row_window_is_still_suppressed() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                label: Some(smol_str::SmolStr::new_static("行情")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");
        let viewport = NodeBuilder::boxed()
            .build(&mut tree, table)
            .expect("viewport");
        declare_virtual_window(&mut tree, viewport, 100, &[]);

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        assert_eq!(
            snapshot_with(&tree, viewport, &mut ids, &mut tables).role,
            A11yRole::GenericContainer,
            "空行窗口（首帧 / 全未挂载）也按行窗口抑制自动 List"
        );
    }

    #[test]
    fn a_list_box_or_menu_container_suppresses_the_inner_list() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let list_box = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ListBox),
                label: Some(smol_str::SmolStr::new_static("选择")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("list box");
        let list_viewport = NodeBuilder::boxed()
            .build(&mut tree, list_box)
            .expect("list viewport");
        let option = NodeBuilder::boxed()
            .build(&mut tree, list_viewport)
            .expect("option");
        declare_virtual_window(&mut tree, list_viewport, 2, &[(0, option)]);

        let menu = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Menu),
                label: Some(smol_str::SmolStr::new_static("交易")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("menu");
        let menu_viewport = NodeBuilder::boxed()
            .build(&mut tree, menu)
            .expect("menu viewport");
        let menu_item = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::MenuItem),
                label: Some(smol_str::SmolStr::new_static("买入")),
                ..A11yOverride::default()
            })
            .build(&mut tree, menu_viewport)
            .expect("menu item");
        declare_virtual_window(&mut tree, menu_viewport, 1, &[(0, menu_item)]);

        let mut ids = A11yIds::new();
        let mut tables = super::CollectionSemanticsCache::default();
        let list_box_record = snapshot_with(&tree, list_box, &mut ids, &mut tables);
        let list_viewport_record = snapshot_with(&tree, list_viewport, &mut ids, &mut tables);
        assert_eq!(list_box_record.role, A11yRole::ListBox);
        assert_eq!(
            list_viewport_record.role,
            A11yRole::GenericContainer,
            "ListBox 包装下的虚拟窗口不得再报 List（否则 AT 读到列表套列表）"
        );
        assert_eq!(
            list_box_record.children,
            vec![list_viewport_record.id],
            "窗口仍是物理子节点，只是语义被透明上提"
        );

        let menu_record = snapshot_with(&tree, menu, &mut ids, &mut tables);
        let menu_viewport_record = snapshot_with(&tree, menu_viewport, &mut ids, &mut tables);
        assert_eq!(menu_record.role, A11yRole::Menu);
        assert_eq!(
            menu_viewport_record.role,
            A11yRole::GenericContainer,
            "Menu 包装下的虚拟窗口不得再报 List"
        );
        assert_eq!(
            snapshot_with(&tree, menu_item, &mut ids, &mut tables).role,
            A11yRole::MenuItem,
            "项角色由行声明决定，不受窗口抑制影响"
        );
    }

    #[test]
    fn orientation_follows_the_patch_first_merge() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let slider = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Slider),
                label: Some(smol_str::SmolStr::new_static("色相")),
                orientation: Some(crate::a11y::A11yOrientation::Horizontal),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("slider");

        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let static_record =
            super::node_a11y(&tree, slider, Scale::ONE, "", &mut ids, &mut diagnostics)
                .expect("slider record");
        assert_eq!(
            static_record.orientation,
            Some(crate::a11y::A11yOrientation::Horizontal)
        );
        assert_eq!(
            crate::a11y::adapter::to_accesskit(&static_record).orientation(),
            Some(accesskit::Orientation::Horizontal)
        );

        tree.set_a11y_patch(
            slider,
            crate::a11y::A11yPatch {
                orientation: Some(crate::a11y::A11yOrientation::Vertical),
                ..crate::a11y::A11yPatch::default()
            },
        );
        let patch_record =
            super::node_a11y(&tree, slider, Scale::ONE, "", &mut ids, &mut diagnostics)
                .expect("slider record after patch");
        assert_eq!(
            patch_record.orientation,
            Some(crate::a11y::A11yOrientation::Vertical),
            "动态补丁必须能覆盖静态声明（patch-first）"
        );
        assert_eq!(
            crate::a11y::adapter::to_accesskit(&patch_record).orientation(),
            Some(accesskit::Orientation::Vertical)
        );
    }
}

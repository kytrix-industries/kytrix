use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::{
    a11y::{A11yAction, A11yDiagnostic, A11yRole},
    node::{
        id::NodeId,
        tree::{HitFocusSkip, Node, NodeTree},
    },
    popup::{HasPopup, PopupEntry, PopupId, PopupStack, PopupState, PopupTier},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct A11yPopupOverlay {
    pub role: Option<A11yRole>,
    pub label: Option<SmolStr>,
    pub hidden: bool,
    pub modal: bool,
    pub expanded: Option<bool>,
    pub has_popup: Option<HasPopup>,
    pub owns: SmallVec<[NodeId; 2]>,
    pub controls: SmallVec<[NodeId; 2]>,
    pub described_by: Option<NodeId>,
    pub actions: SmallVec<[A11yAction; 2]>,
    pub items: SmallVec<[NodeId; 2]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct A11yPopupContext {
    overlays: ahash::HashMap<NodeId, A11yPopupOverlay>,
    logical_parents: ahash::HashMap<NodeId, NodeId>,
    adopted: ahash::HashMap<NodeId, SmallVec<[NodeId; 4]>>,
    modal_scope: SmallVec<[NodeId; 4]>,
}

impl A11yPopupContext {
    pub(crate) fn overlay(&self, node: NodeId) -> Option<&A11yPopupOverlay> {
        self.overlays.get(&node)
    }

    pub(crate) fn logical_parent(&self, node: NodeId) -> Option<NodeId> {
        self.logical_parents.get(&node).copied()
    }

    pub(crate) fn adopted_children(&self, node: NodeId) -> &[NodeId] {
        self.adopted
            .get(&node)
            .map_or(&[][..], |children| children.as_slice())
    }

    pub(crate) fn has_modal_scope(&self) -> bool {
        !self.modal_scope.is_empty()
    }

    pub(crate) fn in_modal_scope(&self, tree: &NodeTree, node: NodeId) -> bool {
        if self.modal_scope.is_empty() {
            return true;
        }
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            if self.modal_scope.contains(&current) {
                return true;
            }
            cursor = tree
                .get(current)
                .ok()
                .and_then(crate::node::tree::Node::parent);
        }
        false
    }

    pub(crate) fn active_descendant(
        &self,
        tree: &NodeTree,
        focus: Option<NodeId>,
        node: NodeId,
    ) -> Option<NodeId> {
        let overlay = self.overlays.get(&node)?;
        let focus = focus?;
        let mut cursor = Some(focus);
        while let Some(current) = cursor {
            if overlay.items.contains(&current) {
                return Some(focus);
            }
            if current == node {
                return None;
            }
            cursor = tree
                .get(current)
                .ok()
                .and_then(crate::node::tree::Node::parent);
        }
        None
    }

    pub(crate) fn active_item_owners(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.overlays
            .iter()
            .filter(|(_, overlay)| !overlay.items.is_empty())
            .map(|(node, _)| *node)
    }

    fn note_overlay(&mut self, node: NodeId, update: impl FnOnce(&mut A11yPopupOverlay)) {
        update(self.overlays.entry(node).or_default());
    }

    fn adopt(&mut self, parent: NodeId, child: NodeId) {
        let children = self.adopted.entry(parent).or_default();
        if !children.contains(&child) {
            children.push(child);
        }
    }
}

pub(crate) fn project(
    tree: &NodeTree,
    stack: &PopupStack,
    diagnostics: &mut Vec<A11yDiagnostic>,
) -> A11yPopupContext {
    let mut context = A11yPopupContext::default();
    let mut topmost_modal: Option<PopupId> = None;
    for id in stack.order() {
        let Ok(entry) = stack.entry(*id) else {
            continue;
        };
        if entry.state == PopupState::Closed {
            continue;
        }
        let dismissable = entry.state.is_dismissable();
        if entry.tier == PopupTier::Scrim {
            if tree.get(entry.node).is_ok() {
                context.note_overlay(entry.node, |overlay| overlay.hidden = true);
            }
            continue;
        }
        if tree.get(entry.node).is_err() {
            continue;
        }
        if entry.is_modal() {
            topmost_modal = Some(*id);
        }
        let modal = entry.is_modal();
        let logical_parent = resolve_logical_parent(tree, stack, entry, diagnostics);
        if let Some(parent) = logical_parent {
            context.logical_parents.insert(entry.node, parent);
            context.adopt(parent, entry.node);
        }
        context.note_overlay(entry.node, |overlay| {
            overlay.role = entry.a11y.role;
            overlay.label = entry.a11y.label.clone();
            overlay.modal = modal;
            overlay.hidden = !dismissable;
            overlay.owns = entry.a11y.owns.clone();
            overlay.items = declared_items(tree, entry);
            if !dismissable {
                return;
            }
            match entry.a11y.role {
                Some(A11yRole::Menu | A11yRole::Dialog | A11yRole::AlertDialog) => {
                    overlay.actions.push(A11yAction::Dismiss);
                }
                Some(A11yRole::Tooltip) => overlay.actions.push(A11yAction::HideTooltip),
                _ => {}
            }
        });
        let has_popup = entry
            .a11y
            .has_popup
            .or_else(|| entry.a11y.role.and_then(derived_has_popup));
        let expanded_target = entry.a11y.expanded_of.or(entry.opener);
        if let Some(target) = expanded_target
            && tree.get(target).is_ok()
        {
            context.note_overlay(target, |overlay| {
                overlay.has_popup = has_popup;
                let composite = matches!(
                    has_popup,
                    Some(HasPopup::Menu | HasPopup::ListBox | HasPopup::Tree | HasPopup::Grid)
                );
                if composite {
                    overlay.expanded = Some(dismissable);
                }
                if dismissable {
                    overlay.controls.push(entry.node);
                }
            });
        }
        if dismissable
            && let Some(target) = entry.a11y.described_by
            && tree.get(target).is_ok()
        {
            context.note_overlay(target, |overlay| {
                overlay.described_by = Some(entry.node);
            });
        }
    }
    if let Some(modal) = topmost_modal
        && let Ok(entry) = stack.entry(modal)
    {
        context.modal_scope.push(entry.node);
        for id in stack.order() {
            let Ok(other) = stack.entry(*id) else {
                continue;
            };
            if *id != modal && other.owner_modal == Some(modal) && tree.get(other.node).is_ok() {
                context.modal_scope.push(other.node);
            }
        }
    }
    context
}

fn declared_items(tree: &NodeTree, entry: &PopupEntry) -> SmallVec<[NodeId; 2]> {
    let mut items = entry.a11y.owns.clone();
    if let Some(patch) = tree.a11y_of(entry.node).map(|slot| &slot.patch)
        && let Some(owns) = &patch.owns
    {
        for node in owns {
            if !items.contains(node) {
                items.push(*node);
            }
        }
    }
    items
}

fn resolve_logical_parent(
    tree: &NodeTree,
    stack: &PopupStack,
    entry: &PopupEntry,
    diagnostics: &mut Vec<A11yDiagnostic>,
) -> Option<NodeId> {
    if let Some(requested) = entry.a11y.a11y_parent {
        match resolve_parent(tree, entry.node, requested) {
            Some(parent) => {
                if parent != requested {
                    diagnostics.push(A11yDiagnostic::InvalidA11yParent {
                        node: entry.node,
                        parent: requested,
                    });
                }
                return Some(parent);
            }
            None => {
                diagnostics.push(A11yDiagnostic::InvalidA11yParent {
                    node: entry.node,
                    parent: requested,
                });
            }
        }
    }
    if entry.is_modal() {
        return None;
    }
    if let Some(parent) = entry.parent
        && let Ok(parent_entry) = stack.entry(parent)
        && let Some(parent) = resolve_parent(tree, entry.node, parent_entry.node)
    {
        return Some(parent);
    }
    entry
        .opener
        .and_then(|opener| resolve_parent(tree, entry.node, opener))
}

pub(crate) fn resolve_parent(tree: &NodeTree, node: NodeId, parent: NodeId) -> Option<NodeId> {
    if parent == node || tree.get(node).is_err() || tree.get(parent).is_err() {
        return None;
    }
    let mut cursor = parent;
    let mut steps = 0usize;
    while tree.hit_focus_skip(cursor) == HitFocusSkip::Container {
        steps += 1;
        if steps > tree.len() {
            return None;
        }
        cursor = tree.get(cursor).ok().and_then(Node::parent)?;
    }
    if cursor == node || is_physical_descendant(tree, node, cursor) {
        return None;
    }
    Some(cursor)
}

pub(crate) fn is_physical_descendant(tree: &NodeTree, ancestor: NodeId, node: NodeId) -> bool {
    let mut cursor = tree.get(node).ok().and_then(Node::parent);
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = tree.get(current).ok().and_then(Node::parent);
    }
    false
}

fn derived_has_popup(role: A11yRole) -> Option<HasPopup> {
    match role {
        A11yRole::Menu => Some(HasPopup::Menu),
        A11yRole::ListBox => Some(HasPopup::ListBox),
        A11yRole::Tree => Some(HasPopup::Tree),
        A11yRole::Grid => Some(HasPopup::Grid),
        A11yRole::Dialog | A11yRole::AlertDialog => Some(HasPopup::Dialog),
        _ => None,
    }
}

pub(crate) fn note_changes(
    previous: &A11yPopupContext,
    next: &A11yPopupContext,
    tree: &mut NodeTree,
) -> bool {
    let mut dirty: SmallVec<[NodeId; 16]> = SmallVec::new();
    for (node, overlay) in &next.overlays {
        if previous.overlays.get(node) != Some(overlay) {
            dirty.push(*node);
        }
    }
    for node in previous.overlays.keys() {
        if !next.overlays.contains_key(node) {
            dirty.push(*node);
        }
    }
    for (node, parent) in &next.logical_parents {
        if previous.logical_parents.get(node) != Some(parent) {
            dirty.push(*node);
            dirty.push(*parent);
        }
    }
    for (node, parent) in &previous.logical_parents {
        if !next.logical_parents.contains_key(node) {
            dirty.push(*node);
            dirty.push(*parent);
        }
    }
    for (parent, children) in &next.adopted {
        if previous.adopted.get(parent) != Some(children) {
            dirty.push(*parent);
        }
    }
    for parent in previous.adopted.keys() {
        if !next.adopted.contains_key(parent) {
            dirty.push(*parent);
        }
    }
    for node in dirty {
        tree.mark_a11y(node);
    }
    previous.modal_scope != next.modal_scope
}

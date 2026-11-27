use crate::{
    a11y::A11yRole,
    app::{App, focus as app_focus},
    interaction::focus,
    node::{id::NodeId, tree::NodeTree},
    popup::{FocusPolicy, FocusSnapshot, InitialFocus, PopupId, RestoreFocus},
    window::WindowId,
};

fn first_focusable(tree: &NodeTree, root: NodeId) -> Option<NodeId> {
    let mut found = None;
    tree.for_each_subtree(root, |node| {
        if found.is_none() && focus::focusable(tree, node) {
            found = Some(node);
        }
    });
    found
}

fn default_button(tree: &NodeTree, root: NodeId) -> Option<NodeId> {
    let mut found = None;
    tree.for_each_subtree(root, |node| {
        if found.is_some() || !focus::focusable(tree, node) {
            return;
        }
        let role = tree
            .a11y_of(node)
            .and_then(|a11y| a11y.over.as_ref())
            .and_then(|over| over.role);
        if role == Some(A11yRole::Button) {
            found = Some(node);
        }
    });
    found
}

fn initial_focus_node(tree: &NodeTree, content: NodeId, policy: FocusPolicy) -> Option<NodeId> {
    match policy {
        FocusPolicy::None => None,
        FocusPolicy::Take { .. } => first_focusable(tree, content).or(Some(content)),
        FocusPolicy::Trap { initial, .. } => match initial {
            InitialFocus::DefaultButton => default_button(tree, content)
                .or_else(|| first_focusable(tree, content))
                .or(Some(content)),
            InitialFocus::FirstFocusable => first_focusable(tree, content).or(Some(content)),
            InitialFocus::Container => Some(content),
        },
    }
}

pub(crate) fn take_focus_on_open(app: &mut App, window: WindowId, id: PopupId, content: NodeId) {
    let previous = app_focus::focused(app, window);
    let (previous_owner, initial, opener, restore) = {
        let Some(target) = app.window(window) else {
            return;
        };
        let stack = target.popups();
        let Ok(entry) = stack.entry(id) else {
            return;
        };
        if matches!(entry.focus, FocusPolicy::None) {
            return;
        }
        let previous_owner = stack.focus_owner().map(|(owner, _)| owner);
        let initial = initial_focus_node(target.tree(), content, entry.focus);
        (previous_owner, initial, entry.opener, entry.focus.restore())
    };
    if let Some(target) = app.window_mut(window) {
        let stack = target.popups_mut();
        stack.push_focus_record(
            id,
            FocusSnapshot {
                node: previous,
                owner: previous_owner,
                opener,
                restore,
            },
        );
        stack.set_focus_owner(Some((id, initial)));
    }
    app_focus::apply_effective_focus(app, window, previous, initial);
}

fn restore_target(snapshot: FocusSnapshot, tree: &NodeTree) -> Option<NodeId> {
    let alive = |node: NodeId| tree.get(node).is_ok();
    match snapshot.restore {
        RestoreFocus::Previous => snapshot.node.filter(|node| alive(*node)),
        RestoreFocus::Opener => match snapshot.opener {
            Some(opener) if alive(opener) => focus::nearest_focusable(tree, opener),
            _ => snapshot.node.filter(|node| alive(*node)),
        },
    }
}

pub(crate) fn release_focus_on_close(app: &mut App, window: WindowId, id: PopupId) {
    let (owns, had_record) = match app.window(window) {
        Some(target) => (
            target
                .popups()
                .focus_owner()
                .is_some_and(|(owner, _)| owner == id),
            target.popups().focus_record_count() > 0,
        ),
        None => return,
    };
    if !owns && !had_record {
        return;
    }
    let previous = app_focus::focused(app, window);
    let mut snapshot: Option<FocusSnapshot> = None;
    if let Some(target) = app.window_mut(window) {
        let stack = target.popups_mut();
        snapshot = stack.take_focus_record(id);
    }
    if !owns {
        return;
    }
    let owner_alive = snapshot
        .and_then(|snapshot| snapshot.owner)
        .is_some_and(|owner| {
            app.window(window)
                .is_some_and(|target| target.popups().entry(owner).is_ok())
        });
    let target_node = snapshot.and_then(|snapshot| {
        let tree = app.window(window)?.tree();
        restore_target(snapshot, tree)
    });
    let restore_main = !owner_alive;
    if let Some(target) = app.window_mut(window) {
        match snapshot.and_then(|snapshot| snapshot.owner) {
            Some(owner) if owner_alive => {
                target
                    .popups_mut()
                    .set_focus_owner(Some((owner, target_node)));
            }
            _ => target.popups_mut().set_focus_owner(None),
        }
        if restore_main {
            target.focus_mut().set(target_node);
        }
    }
    app_focus::apply_effective_focus(app, window, previous, target_node);
}
enum OwnerRepair {
    Clear,
    Focus(PopupId, NodeId),
}

pub(crate) fn validate_owner(app: &mut App, window: WindowId) {
    let repair = (|| {
        let target = app.window(window)?;
        let stack = target.popups();
        let (owner, node) = stack.focus_owner()?;
        let tree = target.tree();
        let Ok(content) = stack.entry(owner).map(|entry| entry.node) else {
            return Some(OwnerRepair::Clear);
        };
        if tree.get(content).is_err() {
            return Some(OwnerRepair::Clear);
        }
        let valid = node.is_some_and(|node| {
            tree.get(node).is_ok()
                && match stack.topmost_modal() {
                    Some(modal) => stack.node_in_popup_scope(tree, node, modal),
                    None => stack.node_in_focus_scope(tree, node, owner),
                }
        });
        if valid {
            return None;
        }
        let fallback = match stack.topmost_modal() {
            Some(modal) => {
                let mut candidate = None;
                tree.for_each_subtree(content, |candidate_node| {
                    if candidate.is_none()
                        && stack.node_in_popup_scope(tree, candidate_node, modal)
                        && focus::focusable(tree, candidate_node)
                    {
                        candidate = Some(candidate_node);
                    }
                });
                candidate.unwrap_or(content)
            }
            None => first_focusable(tree, content).unwrap_or(content),
        };
        Some(OwnerRepair::Focus(owner, fallback))
    })();
    match repair {
        None => {}
        Some(OwnerRepair::Clear) => {
            let previous = app_focus::focused(app, window);
            if let Some(target) = app.window_mut(window) {
                target.popups_mut().set_focus_owner(None);
            }
            let next = app_focus::focused(app, window);
            app_focus::apply_effective_focus(app, window, previous, next);
        }
        Some(OwnerRepair::Focus(owner, fallback)) => {
            let previous = app_focus::focused(app, window);
            if let Some(target) = app.window_mut(window) {
                target.popups_mut().set_owner_node(owner, Some(fallback));
            }
            app_focus::apply_effective_focus(app, window, previous, Some(fallback));
        }
    }
}

pub(crate) fn set_owner_focus(app: &mut App, window: WindowId, node: Option<NodeId>) -> bool {
    let (owner, valid) = {
        let Some(target) = app.window(window) else {
            return false;
        };
        let stack = target.popups();
        let Some((owner, _)) = stack.focus_owner() else {
            return false;
        };
        let tree = target.tree();
        let valid = node.is_none_or(|node| {
            let in_scope = match stack.topmost_modal() {
                Some(modal) => stack.node_in_popup_scope(tree, node, modal),
                None => stack.node_in_focus_scope(tree, node, owner),
            };
            in_scope
                && (focus::focusable(tree, node)
                    || (focus::editable_focusable(tree, node) && in_scope))
        });
        (owner, valid)
    };
    if !valid {
        return false;
    }
    let previous = app_focus::focused(app, window);
    if let Some(target) = app.window_mut(window) {
        target.popups_mut().set_owner_node(owner, node);
    }
    app_focus::apply_effective_focus(app, window, previous, node);
    true
}

pub(crate) fn effective_focus(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.window(window)?;
    let stack = target.popups();
    let (_, node) = stack.focus_owner()?;
    let tree = target.tree();
    node.filter(|node| tree.get(*node).is_ok())
}

pub(crate) fn next_in_popup_scope(
    app: &App,
    window: WindowId,
    from: Option<NodeId>,
    backwards: bool,
) -> Option<NodeId> {
    let target = app.window(window)?;
    let stack = target.popups();
    let tree = target.tree();
    let modal = stack.topmost_modal();
    let owner = stack.focus_owner().map(|(owner, _)| owner);
    let candidate = |node: &NodeId| {
        let in_scope = match modal {
            Some(modal) => stack.node_in_popup_scope(tree, *node, modal),
            None => owner.is_some_and(|owner| stack.node_in_focus_scope(tree, *node, owner)),
        };
        in_scope && focus::focusable(tree, *node)
    };
    let order = tree.dfs_ids();
    let start = from
        .filter(|from| candidate(from))
        .and_then(|from| order.iter().position(|node| *node == from));
    let Some(start) = start else {
        return if backwards {
            order.iter().rev().copied().find(candidate)
        } else {
            order.iter().copied().find(candidate)
        };
    };
    let len = order.len();
    for step in 1..=len {
        let index = if backwards {
            (start + len - step) % len
        } else {
            (start + step) % len
        };
        if candidate(&order[index]) {
            return Some(order[index]);
        }
    }
    None
}

pub(crate) fn next_in_owner_scope(
    app: &App,
    window: WindowId,
    from: Option<NodeId>,
    backwards: bool,
) -> Option<NodeId> {
    let target = app.window(window)?;
    let stack = target.popups();
    let tree = target.tree();
    let (owner, _) = stack.focus_owner()?;
    let candidate = |node: &NodeId| {
        stack.node_in_focus_scope(tree, *node, owner) && focus::focusable(tree, *node)
    };
    let order = tree.dfs_ids();
    let start = from.and_then(|from| order.iter().position(|node| *node == from))?;
    if backwards {
        order[..start].iter().rev().copied().find(candidate)
    } else {
        order[start + 1..].iter().copied().find(candidate)
    }
}

pub(crate) fn leave_owner_scope(
    app: &mut App,
    window: WindowId,
    backwards: bool,
) -> Option<NodeId> {
    let anchor = {
        let target = app.window(window)?;
        let stack = target.popups();
        let (owner, _) = stack.focus_owner()?;
        stack.focus_record(owner).and_then(|record| record.node)
    };
    let previous = app_focus::focused(app, window);
    let next = {
        let target = app.window(window)?;
        let tree = target.tree();
        anchor
            .filter(|node| tree.get(*node).is_ok())
            .and_then(|anchor| focus::next_focusable(tree, Some(anchor), backwards))
    };
    if let Some(target) = app.window_mut(window) {
        target.popups_mut().set_focus_owner(None);
        target.focus_mut().set(next);
    }
    app_focus::apply_effective_focus(app, window, previous, next);
    next
}

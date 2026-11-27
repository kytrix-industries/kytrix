use super::App;
use crate::{
    error::EngineError,
    interaction::{focus, scroll},
    math::LogicalRect,
    node::id::NodeId,
    popup::focus as popup_focus,
    window::{Window, WindowId},
};

#[must_use]
pub(crate) fn focused(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    if target.popups().focus_owner().is_some() {
        return popup_focus::effective_focus(app, window);
    }
    let node = target.focus().focused()?;
    target.tree().get(node).is_ok().then_some(node)
}

#[must_use]
pub(crate) fn focus_scope(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let modal = target.popups().topmost_modal()?;
    target.popups().entry(modal).ok().map(|entry| entry.node)
}

#[must_use]
pub(crate) fn in_input_scope(app: &App, window: WindowId, node: NodeId) -> bool {
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    let tree = target.tree();
    match target.popups().topmost_modal() {
        Some(modal) => target.popups().node_in_popup_scope(tree, node, modal),
        None => true,
    }
}

#[must_use]
pub(crate) fn scope_active(app: &App, window: WindowId) -> bool {
    app.windows
        .get(&window)
        .is_some_and(|target| target.popups().topmost_modal().is_some())
}

#[must_use]
pub(crate) fn hovered(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.pointer().hovered?;
    in_input_scope(app, window, node).then_some(node)
}

pub(crate) fn apply_effective_focus(
    app: &mut App,
    window: WindowId,
    previous: Option<NodeId>,
    next: Option<NodeId>,
) {
    if previous == next {
        return;
    }
    {
        let Some(target) = app.windows.get_mut(&window) else {
            return;
        };
        for changed in [previous, next].into_iter().flatten() {
            crate::text_edit::apply_focus(target.tree_mut(), changed, Some(changed) == next);
            let has_ring = target.tree().focus_ring(changed).is_some();
            target.tree_mut().mark_a11y(changed);
            if has_ring {
                target.tree_mut().mark_paint(changed);
            }
        }
        let mut ring_damage = LogicalRect::default();
        for changed in [previous, next].into_iter().flatten() {
            if let Some(ring) = target.tree().focus_ring(changed)
                && let Some(screen) = target.tree().node_screen_rect(changed)
            {
                ring_damage = ring_damage.union(screen.outset(ring.offset + ring.width));
            }
        }
        if !ring_damage.is_empty() {
            target.request_damage(ring_damage);
        }
    }
    if let Some(previous) = previous {
        let _ = crate::text_edit::cancel_composition(app, window, previous);
    }
    crate::text_edit::cancel_pointer_press(app, window);
    crate::text_edit::note_focus_change(app, window);
    app.request_frame();
}

pub(crate) fn validate_focus(app: &mut App, window: WindowId) {
    {
        let Some(target) = app.windows.get(&window) else {
            return;
        };
        if target.popups().focus_owner().is_some() {
            popup_focus::validate_owner(app, window);
            return;
        }
    }
    let stale = app.windows.get(&window).is_some_and(|target| {
        target
            .focus()
            .focused()
            .is_some_and(|node| target.tree().get(node).is_err())
    });
    if stale && let Some(target) = app.windows.get_mut(&window) {
        target.focus_mut().set(None);
    }
}

pub(crate) fn set_focus(app: &mut App, window: WindowId, node: Option<NodeId>) -> bool {
    if app
        .windows
        .get(&window)
        .is_some_and(|target| target.popups().focus_owner().is_some())
    {
        return popup_focus::set_owner_focus(app, window, node);
    }
    let previous = focused(app, window);
    {
        let Some(target) = app.windows.get_mut(&window) else {
            return false;
        };
        if let Some(node) = node
            && !focus::focusable(target.tree(), node)
            && !focus::editable_focusable(target.tree(), node)
        {
            return false;
        }
        let old = target.focus().focused();
        if !target.focus_mut().set(node) {
            return false;
        }
        if let Some(previous) = old
            && let Some(ring) = target.tree().focus_ring(previous)
            && let Ok(slot) = target.tree().get(previous)
        {
            let bounds = slot.layout().bounds;
            let screen =
                scroll::to_screen(target.tree(), previous, bounds).outset(ring.offset + ring.width);
            target.request_damage(screen);
        }
        for changed in [old, node].into_iter().flatten() {
            crate::text_edit::apply_focus(target.tree_mut(), changed, Some(changed) == node);
            let has_ring = target.tree().focus_ring(changed).is_some();
            target.tree_mut().mark_a11y(changed);
            if has_ring {
                target.tree_mut().mark_paint(changed);
            }
        }
    }
    if let Some(previous) = previous
        && Some(previous) != node
    {
        let _ = crate::text_edit::cancel_composition(app, window, previous);
    }
    crate::text_edit::cancel_pointer_press(app, window);
    crate::text_edit::note_focus_change(app, window);
    app.request_frame();
    true
}

pub(crate) fn set_ime_cursor_area(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    byte_index: usize,
) -> Result<(), EngineError> {
    let changed = {
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        let Some(slot) = target.tree().get(node).ok() else {
            return Err(EngineError::StaleNode { node });
        };
        if slot.kind().text().is_none() {
            return Err(EngineError::TextShape {
                node,
                detail: smol_str::SmolStr::new_static("node is not a text node"),
            });
        }
        target.set_ime_cursor_request(node, byte_index)
    };
    if changed {
        app.dispatcher.wake();
    }
    Ok(())
}

pub(crate) fn clear_ime_cursor_area(app: &mut App, window: WindowId) {
    let changed = app
        .windows
        .get_mut(&window)
        .is_some_and(Window::clear_ime_cursor_request);
    if changed {
        app.dispatcher.wake();
    }
}

pub(crate) fn set_ime_enabled(app: &mut App, window: WindowId, enabled: bool) -> bool {
    if !app.windows.contains_key(&window) {
        return false;
    }
    let changed = if enabled {
        app.ime_disabled.remove(&window)
    } else {
        app.ime_disabled.insert(window)
    };
    if changed {
        app.dispatcher.wake();
    }
    changed
}

pub(crate) fn ime_cursor_area(
    app: &App,
    window: WindowId,
) -> Result<Option<LogicalRect>, EngineError> {
    app.windows
        .get(&window)
        .ok_or(EngineError::UnknownWindow { window })?
        .ime_cursor_area()
}

pub(crate) fn focus_next(app: &mut App, window: WindowId) -> Option<NodeId> {
    app.validate_focus(window);
    let (modal, owner) = app.windows.get(&window).map_or((false, false), |target| {
        (
            target.popups().topmost_modal().is_some(),
            target.popups().focus_owner().is_some(),
        )
    });
    let focused = app.focused(window);
    let next = if modal {
        popup_focus::next_in_popup_scope(app, window, focused, false)
    } else if owner {
        match popup_focus::next_in_owner_scope(app, window, focused, false) {
            Some(next) => Some(next),
            None => popup_focus::leave_owner_scope(app, window, false),
        }
    } else {
        app.windows.get(&window).and_then(|target| {
            focus::next_focusable(target.tree(), target.focus().focused(), false)
        })
    };
    if app.set_focus(window, next) {
        next
    } else {
        None
    }
}

pub(crate) fn focus_prev(app: &mut App, window: WindowId) -> Option<NodeId> {
    app.validate_focus(window);
    let (modal, owner) = app.windows.get(&window).map_or((false, false), |target| {
        (
            target.popups().topmost_modal().is_some(),
            target.popups().focus_owner().is_some(),
        )
    });
    let focused = app.focused(window);
    let next = if modal {
        popup_focus::next_in_popup_scope(app, window, focused, true)
    } else if owner {
        match popup_focus::next_in_owner_scope(app, window, focused, true) {
            Some(next) => Some(next),
            None => popup_focus::leave_owner_scope(app, window, true),
        }
    } else {
        app.windows
            .get(&window)
            .and_then(|target| focus::next_focusable(target.tree(), target.focus().focused(), true))
    };
    if app.set_focus(window, next) {
        next
    } else {
        None
    }
}

pub(crate) fn ime_enabled(app: &App, window: WindowId) -> bool {
    !app.ime_disabled.contains(&window)
}

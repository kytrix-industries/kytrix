use super::App;
use crate::{
    error::EngineError,
    interaction::{focus, overlay as overlay_util, scroll},
    math::LogicalRect,
    node::id::NodeId,
    window::{Window, WindowId},
};

#[must_use]
pub(crate) fn focused(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.focus().focused()?;
    let tree = target.tree();
    if tree.get(node).is_err() {
        return None;
    }
    let scope = overlay_util::focus_scope_root(tree, tree.root());
    focus::in_scope(tree, node, scope).then_some(node)
}

#[must_use]
pub(crate) fn focus_scope(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let tree = target.tree();
    overlay_util::focus_scope_root(tree, tree.root())
}

#[must_use]
pub(crate) fn hovered(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.pointer().hovered?;
    let tree = target.tree();
    if tree.get(node).is_err() {
        return None;
    }
    let scope = overlay_util::focus_scope_root(tree, tree.root());
    overlay_util::in_modal_scope(tree, node, scope).then_some(node)
}

pub(crate) fn validate_focus(app: &mut App, window: WindowId) {
    let repair = {
        let Some(target) = app.windows.get(&window) else {
            return;
        };
        let tree = target.tree();
        let scope = overlay_util::focus_scope_root(tree, tree.root());
        let first_in_scope =
            || scope.and_then(|scope| focus::next_focusable(tree, None, false, Some(scope)));
        match target.focus().focused() {
            None => first_in_scope().map(Some),
            Some(node) if tree.get(node).is_err() => Some(None),
            Some(node) if scope.is_some() && !focus::in_scope(tree, node, scope) => {
                first_in_scope().map(Some)
            }
            Some(_) => None,
        }
    };
    match repair {
        Some(None) => {
            if let Some(target) = app.windows.get_mut(&window) {
                target.focus_mut().set(None);
            }
        }
        Some(Some(next)) => {
            app.set_focus(window, Some(next));
        }
        None => {}
    }
}

pub(crate) fn set_focus(app: &mut App, window: WindowId, node: Option<NodeId>) -> bool {
    let scope = focus_scope(app, window);
    let focus_changed = {
        let Some(target) = app.windows.get_mut(&window) else {
            return false;
        };
        if let Some(node) = node
            && !focus::focusable_in_scope(target.tree(), node, scope)
            && !(focus::editable_focusable(target.tree(), node)
                && focus::in_scope(target.tree(), node, scope))
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
        old != node
    };
    if focus_changed {
        crate::text_edit::cancel_pointer_press(app, window);
    }
    crate::text_edit::note_focus_change(app, window);
    app.request_frame();
    true
}

pub(crate) fn restore_focus(app: &mut App, window: WindowId, opener: Option<NodeId>) -> bool {
    let Some(opener) = opener else {
        return false;
    };
    let alive = app
        .windows
        .get(&window)
        .is_some_and(|target| target.tree().get(opener).is_ok());
    if !alive {
        return false;
    }
    app.set_focus(window, Some(opener))
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
    let scope = focus_scope(app, window);
    let next = app.windows.get(&window).and_then(|target| {
        focus::next_focusable(target.tree(), target.focus().focused(), false, scope)
    });
    if app.set_focus(window, next) {
        next
    } else {
        None
    }
}

pub(crate) fn focus_prev(app: &mut App, window: WindowId) -> Option<NodeId> {
    app.validate_focus(window);
    let scope = focus_scope(app, window);
    let next = app.windows.get(&window).and_then(|target| {
        focus::next_focusable(target.tree(), target.focus().focused(), true, scope)
    });
    if app.set_focus(window, next) {
        next
    } else {
        None
    }
}

pub(crate) fn ime_enabled(app: &App, window: WindowId) -> bool {
    !app.ime_disabled.contains(&window)
}

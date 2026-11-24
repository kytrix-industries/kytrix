use std::time::Instant;

use smallvec::SmallVec;

use super::{App, ClickRecord};
use crate::{
    event::{
        Action, ActionSource, Event, InputEvent, InputOutcome, KeyCode, KeyState, Modifiers,
        PointerButton, ScrollPhase, ScrollUnit,
    },
    interaction::{
        ActionCtx, DragInfo, DragPhase, DragSession, DragState, InputThresholds, PendingDrag,
        PressRecord, ScrollHandle, drag as drag_util, focus, overlay as overlay_util, scroll,
    },
    layout::hit_test::HitResult,
    math::{LogicalPoint, LogicalPx, px},
    node::id::NodeId,
    paint::style::Cursor,
    window::{Window, WindowId},
};

pub(crate) fn set_input_thresholds(app: &mut App, thresholds: InputThresholds) {
    app.thresholds = thresholds;
}

pub(crate) fn dispatch_action(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    action: Action,
    source: ActionSource,
) -> bool {
    if !matches!(
        action,
        Action::FocusNext | Action::FocusPrev | Action::Dismiss
    ) {
        let scope = app.modal_scope(window);
        let in_scope = app
            .windows
            .get(&window)
            .is_some_and(|target| focus::in_scope(target.tree(), node, scope));
        if !in_scope {
            return false;
        }
    }
    app.dispatch_action_with_hit(window, node, action, source, None, Modifiers::empty())
}

pub(crate) fn dispatch_action_focused(
    app: &mut App,
    window: WindowId,
    action: Action,
    source: ActionSource,
) -> bool {
    let Some(node) = app.focused(window) else {
        return false;
    };
    app.dispatch_action(window, node, action, source)
}

pub(crate) fn dispatch_action_with_hit(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    action: Action,
    source: ActionSource,
    hit: Option<HitResult>,
    modifiers: Modifiers,
) -> bool {
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    if target.tree().get(node).is_err() {
        return false;
    }
    if let Some(handler) = app.handlers.remove(&action) {
        let mut guard = HandlerGuard {
            app,
            action,
            handler: Some(handler),
        };
        {
            let mut ctx = ActionCtx::new(guard.app, window, node, hit, modifiers, source);
            if let Some(handler) = guard.handler.as_mut() {
                handler(&mut ctx);
            }
        }
        return true;
    }
    match action {
        Action::FocusNext => {
            app.focus_next(window);
            true
        }
        Action::FocusPrev => {
            app.focus_prev(window);
            true
        }
        Action::Dismiss => app.dismiss_overlay(window),
        Action::Custom(_) => false,
    }
}

#[cfg(any(test, feature = "test-support", feature = "automation"))]
pub(crate) fn dispatch_input(app: &mut App, window: WindowId, event: InputEvent) -> InputOutcome {
    dispatch_input_tracked(app, window, event).0
}

pub(crate) fn dispatch_input_tracked(
    app: &mut App,
    window: WindowId,
    event: InputEvent,
) -> (InputOutcome, bool) {
    let mut outcome = InputOutcome::default();
    let mut consumed = false;
    if !app.windows.contains_key(&window) {
        return (outcome, consumed);
    }
    let position = match &event {
        InputEvent::PointerEnter { position, .. }
        | InputEvent::PointerMove { position, .. }
        | InputEvent::PointerDown { position, .. }
        | InputEvent::PointerUp { position, .. }
        | InputEvent::PointerLeave { position, .. }
        | InputEvent::Scroll { position, .. } => Some(*position),
        _ => None,
    };
    if let Some(position) = position
        && let Some(target) = app.windows.get_mut(&window)
    {
        target.note_pointer_position(position);
    }
    app.validate_focus(window);
    match event {
        InputEvent::PointerEnter {
            position,
            modifiers,
            ..
        }
        | InputEvent::PointerMove {
            position,
            modifiers,
            ..
        } => app.on_pointer_move(window, position, modifiers, &mut outcome),
        InputEvent::PointerDown {
            position,
            button,
            modifiers,
            timestamp,
        } => app.on_pointer_down(window, position, button, modifiers, timestamp, &mut outcome),
        InputEvent::PointerUp {
            position,
            button,
            modifiers,
            timestamp,
        } => app.on_pointer_up(window, position, button, modifiers, timestamp, &mut outcome),
        InputEvent::PointerLeave { modifiers, .. } => app.on_pointer_leave(window, modifiers),
        InputEvent::PointerCancel { .. } => app.cancel_pointer_session(window),
        InputEvent::Scroll {
            position,
            delta,
            unit,
            phase,
            modifiers,
            ..
        } => app.on_scroll(
            window,
            position,
            delta,
            unit,
            phase,
            modifiers,
            &mut outcome,
        ),
        InputEvent::Key {
            code,
            state,
            repeat,
            modifiers,
            ..
        } => {
            consumed = app.on_key(window, code, state, repeat, modifiers, &mut outcome);
        }
        InputEvent::Text { text, .. } => {
            consumed = crate::text_edit::handle_text(app, window, &text);
        }
        InputEvent::Magnify { .. }
        | InputEvent::Rotate { .. }
        | InputEvent::SmartMagnify { .. } => {}
    }
    (outcome, consumed)
}

pub(crate) fn hit_at(app: &App, window: WindowId, position: LogicalPoint) -> Option<HitResult> {
    app.windows
        .get(&window)
        .and_then(|target| target.tree().hit_test(position))
}

pub(crate) fn on_pointer_down(
    app: &mut App,
    window: WindowId,
    position: LogicalPoint,
    button: PointerButton,
    modifiers: Modifiers,
    timestamp: Instant,
    outcome: &mut InputOutcome,
) {
    if app
        .windows
        .get(&window)
        .is_some_and(|target| target.pointer().drag.is_some())
    {
        return;
    }
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let outside = app.windows.get(&window).and_then(|target| {
        let tree = target.tree();
        overlay_util::outside_click_target(tree, tree.root(), hit.map(|result| result.node))
    });
    if let Some((node, _)) = outside {
        if let Some(target) = app.windows.get_mut(&window) {
            target.pointer_mut().press = None;
            target.pointer_mut().pending_drag = None;
        }
        let _ = app.remove_node(window, node);
        return;
    }
    if let Some(scope) = app.modal_scope(window) {
        let inside = app.windows.get(&window).is_some_and(|target| {
            hit.is_some_and(|result| {
                overlay_util::in_modal_scope(target.tree(), result.node, Some(scope))
            })
        });
        if !inside {
            return;
        }
    }
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        pointer.press = Some(PressRecord {
            node: hit.map(|result| result.node),
            position,
            button,
            at: timestamp,
        });
    }
    update_hover(app, window, hit.map(|result| result.node));
    if button.is_primary() {
        let focus_target = app.windows.get(&window).and_then(|target| {
            hit.and_then(|result| focus::nearest_focusable(target.tree(), result.node))
        });
        app.set_focus(window, focus_target);
    }
    if button.is_primary() {
        let drag = app.windows.get(&window).and_then(|target| {
            hit.and_then(|result| focus::drag_target(target.tree(), result.node))
        });
        if let Some((node, action)) = drag
            && let Some(target) = app.windows.get_mut(&window)
        {
            target.pointer_mut().pending_drag = Some(PendingDrag {
                node,
                action,
                origin: position,
            });
        }
    }
}

pub(crate) fn on_pointer_move(
    app: &mut App,
    window: WindowId,
    position: LogicalPoint,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let thresholds = app.thresholds;
    let mut promote: Option<(NodeId, Action, LogicalPoint)> = None;
    let mut moved: Option<(NodeId, Action, LogicalPoint)> = None;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        if let Some(session) = pointer.drag.as_mut() {
            session.current = position;
            session.phase = DragPhase::Move;
            moved = Some((session.node, session.action.clone(), session.delta()));
        } else if let Some(pending) = pointer.pending_drag.as_ref() {
            let dx = position.x - pending.origin.x;
            let dy = position.y - pending.origin.y;
            if drag_util::crosses_threshold(LogicalPoint::new(dx, dy), thresholds.drag_distance) {
                let pending = pointer.pending_drag.take().expect("刚检查过");
                pointer.drag = Some(DragSession {
                    node: pending.node,
                    action: pending.action.clone(),
                    origin: pending.origin,
                    current: position,
                    phase: DragPhase::Start,
                });
                pointer.capture = Some(pending.node);
                promote = Some((pending.node, pending.action, LogicalPoint::new(dx, dy)));
            }
        }
    }
    update_hover(app, window, hit.map(|result| result.node));
    update_cursor(app, window, hit);
    if let Some((node, action, delta)) = promote {
        app.capture_requests.insert(window, true);
        if app.dispatch_action_with_hit(
            window,
            node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::Start,
                delta,
            },
            hit,
            modifiers,
        ) {
            outcome.action = Some(action);
            outcome.target = Some(node);
        }
        return;
    }
    if let Some((node, action, delta)) = moved
        && app.dispatch_action_with_hit(
            window,
            node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::Move,
                delta,
            },
            hit,
            modifiers,
        )
    {
        outcome.action = Some(action);
        outcome.target = Some(node);
    }
}

pub(crate) fn on_pointer_up(
    app: &mut App,
    window: WindowId,
    position: LogicalPoint,
    button: PointerButton,
    modifiers: Modifiers,
    timestamp: Instant,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let session = app.windows.get_mut(&window).and_then(|target| {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        if !button.is_primary() {
            return None;
        }
        pointer.pending_drag = None;
        pointer.end_drag()
    });
    if let Some(session) = session {
        app.capture_requests.insert(window, false);
        if let Some(target) = app.windows.get_mut(&window) {
            target.pointer_mut().press = None;
        }
        let delta = session.delta();
        let action = session.action;
        if app.dispatch_action_with_hit(
            window,
            session.node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::End,
                delta,
            },
            hit,
            modifiers,
        ) {
            outcome.action = Some(action);
            outcome.target = Some(session.node);
        }
        return;
    }
    let press = app.windows.get_mut(&window).and_then(|target| {
        let pointer = target.pointer_mut();
        if pointer.press.is_some_and(|press| press.button == button) {
            pointer.press.take()
        } else {
            None
        }
    });
    let Some(press) = press else {
        return;
    };
    if !button.is_primary() {
        return;
    }
    let Some(press_node) = press.node else {
        return;
    };
    let moved = LogicalPoint::new(position.x - press.position.x, position.y - press.position.y);
    if drag_util::crosses_threshold(moved, app.thresholds.drag_distance) {
        return;
    }
    if hit.map(|result| result.node) != Some(press_node) {
        return;
    }
    let count = app.click_count(window, press_node, position, button, timestamp);
    outcome.click_count = count;
    let target_action = app
        .windows
        .get(&window)
        .and_then(|target| focus::action_target(target.tree(), press_node));
    if let Some((node, action)) = target_action {
        let handled = app.dispatch_action_with_hit(
            window,
            node,
            action.clone(),
            ActionSource::Click {
                click_count: count,
                button,
            },
            hit,
            modifiers,
        );
        if handled {
            outcome.action = Some(action);
            outcome.target = Some(node);
        }
    }
}

pub(crate) fn on_pointer_leave(app: &mut App, window: WindowId, modifiers: Modifiers) {
    let mut changed = false;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        let capture = pointer.capture.is_some();
        if !capture {
            pointer.press = None;
            pointer.pending_drag = None;
        }
        if pointer.cursor != Cursor::Default {
            pointer.cursor = Cursor::Default;
            changed = true;
        }
    }
    update_hover(app, window, None);
    if changed {
        app.cursor_pending = Some((window, Cursor::Default));
    }
}

pub(crate) fn on_scroll(
    app: &mut App,
    window: WindowId,
    position: LogicalPoint,
    delta: LogicalPoint,
    unit: ScrollUnit,
    phase: ScrollPhase,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let Some(result) = hit else {
        return;
    };
    if let Some(scope) = app.modal_scope(window) {
        let inside = app.windows.get(&window).is_some_and(|target| {
            overlay_util::in_modal_scope(target.tree(), result.node, Some(scope))
        });
        if !inside {
            return;
        }
    }
    let (delta_x, delta_y) = if modifiers.contains(Modifiers::SHIFT) {
        (delta.y, delta.x)
    } else {
        (delta.x, delta.y)
    };
    if delta_x == LogicalPx::ZERO && delta_y == LogicalPx::ZERO {
        return;
    }
    let direct = unit == ScrollUnit::Pixel || phase.is_continuous();
    let Some(target) = app.windows.get(&window) else {
        return;
    };
    let mut chain: SmallVec<
        [(
            NodeId,
            crate::interaction::ScrollSlot,
            crate::math::LogicalSize,
        ); 8],
    > = SmallVec::new();
    let mut cursor = Some(result.node);
    while let Some(current) = cursor {
        if let Some(slot) = target.tree().scroll_slot(current) {
            let size = scroll::viewport(target.tree(), current).size;
            chain.push((current, slot, size));
        }
        cursor = target
            .tree()
            .get(current)
            .ok()
            .and_then(|node| node.parent());
    }
    let mut dx = delta_x;
    let mut dy = delta_y;
    for (node, slot, size) in chain {
        let drives = match slot.axis {
            crate::interaction::ScrollAxis::Vertical => dy,
            crate::interaction::ScrollAxis::Horizontal => dx,
        };
        if drives == LogicalPx::ZERO {
            continue;
        }
        let max = scroll::max_offset(&slot, size);
        let pending = if direct {
            let handle = ScrollHandle::new(window, node);
            super::animation::cancel_scroll_animation(app, &handle);
            slot.offset
        } else {
            app.scroll_animation_target(node, window)
                .unwrap_or(slot.offset)
        };
        let (before, limit) = match slot.axis {
            crate::interaction::ScrollAxis::Vertical => (pending.y, max.y),
            crate::interaction::ScrollAxis::Horizontal => (pending.x, max.x),
        };
        let before = px(before.value().clamp(0.0, limit.value()));
        let step = if unit == ScrollUnit::Line {
            slot.line_height
        } else {
            px(1.0)
        };
        let after = px((before - step * drives.value())
            .value()
            .clamp(0.0, limit.value()));
        let consumed = px((before - after) / step);
        if consumed == LogicalPx::ZERO {
            continue;
        }
        let handle = ScrollHandle::new(window, node);
        let mut goal = LogicalPoint::new(px(0.0), px(0.0));
        match slot.axis {
            crate::interaction::ScrollAxis::Vertical => goal.y = after,
            crate::interaction::ScrollAxis::Horizontal => goal.x = after,
        }
        if direct {
            let _ = app.scroll_to(&handle, goal);
        } else {
            let _ = app.scroll_to_animated(&handle, goal);
        }
        match slot.axis {
            crate::interaction::ScrollAxis::Vertical => dy -= consumed,
            crate::interaction::ScrollAxis::Horizontal => dx -= consumed,
        }
        if dx == LogicalPx::ZERO && dy == LogicalPx::ZERO {
            break;
        }
    }
}

pub(crate) fn on_key(
    app: &mut App,
    window: WindowId,
    code: KeyCode,
    state: KeyState,
    repeat: bool,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) -> bool {
    match state {
        KeyState::Down => {
            app.held_keys.entry(window).or_default().insert(code);
        }
        KeyState::Up => {
            if let Some(held) = app.held_keys.get_mut(&window) {
                held.remove(&code);
            }
            return app
                .consumed_keys
                .get_mut(&window)
                .is_some_and(|consumed| consumed.remove(&code));
        }
    }
    let consumed = crate::text_edit::handle_key(app, window, code, repeat, modifiers);
    {
        let consumed_keys = app.consumed_keys.entry(window).or_default();
        if consumed {
            consumed_keys.insert(code);
        } else {
            consumed_keys.remove(&code);
        }
    }
    if consumed {
        return true;
    }
    if !repeat {
        if matches!(code, KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space) {
            let focused = app.focused(window);
            let scope = app.modal_scope(window);
            let activation = app.windows.get(&window).and_then(|target| {
                focused
                    .filter(|node| focus::in_scope(target.tree(), *node, scope))
                    .and_then(|node| focus::action_target(target.tree(), node))
            });
            if let Some((node, action)) = activation {
                let handled = app.dispatch_action_with_hit(
                    window,
                    node,
                    action.clone(),
                    ActionSource::Click {
                        click_count: 1,
                        button: PointerButton::Primary,
                    },
                    None,
                    modifiers,
                );
                if handled {
                    outcome.action = Some(action);
                    outcome.target = Some(node);
                }
                return false;
            }
        }
        let focused = app.focused(window);
        let mut scoped_actions: SmallVec<[Action; 4]> = SmallVec::new();
        if let Some(focused) = focused
            && let Some(tree) = app.windows.get(&window).map(Window::tree)
        {
            let mut cursor = Some(focused);
            while let Some(scope) = cursor {
                if let Some(action) = app
                    .scoped_bindings
                    .iter()
                    .find(|scoped| {
                        scoped.window == window
                            && scoped.scope == scope
                            && scoped.binding.key == code
                            && scoped.binding.modifiers == modifiers
                    })
                    .map(|scoped| scoped.binding.action.clone())
                {
                    scoped_actions.push(action);
                }
                cursor = tree.get(scope).ok().and_then(|target| target.parent());
            }
        }
        if let Some(node) = focused {
            for action in scoped_actions {
                if app.dispatch_action_with_hit(
                    window,
                    node,
                    action.clone(),
                    ActionSource::Key,
                    None,
                    modifiers,
                ) {
                    outcome.action = Some(action);
                    outcome.target = Some(node);
                    return false;
                }
            }
        }
        let binding = app
            .bindings
            .iter()
            .find(|binding| binding.key == code && binding.modifiers == modifiers)
            .map(|binding| binding.action.clone());
        if let Some(action) = binding {
            let node =
                focused.or_else(|| app.windows.get(&window).map(|target| target.tree().root()));
            let Some(node) = node else {
                return false;
            };
            if app.dispatch_action_with_hit(
                window,
                node,
                action.clone(),
                ActionSource::Key,
                None,
                modifiers,
            ) {
                outcome.action = Some(action);
                outcome.target = Some(node);
            }
            return false;
        }
    }
    if modifiers.is_empty() {
        default_key_scroll(app, window, code);
    }
    false
}

fn default_key_scroll(app: &mut App, window: WindowId, code: KeyCode) -> bool {
    use crate::{event::Key, interaction::ScrollAxis};
    let Some(key) = Key::from_code(code) else {
        return false;
    };
    let Some(focused) = app.focused(window) else {
        return false;
    };
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    let Some((node, slot)) = scroll::scroll_ancestor(target.tree(), focused) else {
        return false;
    };
    let viewport = scroll::viewport(target.tree(), node).size;
    let max = scroll::max_offset(&slot, viewport);
    let limit = match slot.axis {
        ScrollAxis::Vertical => max.y,
        ScrollAxis::Horizontal => max.x,
    };
    let line = slot.line_height;
    let page = match slot.axis {
        ScrollAxis::Vertical => viewport.height,
        ScrollAxis::Horizontal => viewport.width,
    };
    let goal = match (slot.axis, key) {
        (ScrollAxis::Vertical, Key::ArrowUp) => Some(-line),
        (ScrollAxis::Vertical, Key::ArrowDown) => Some(line),
        (ScrollAxis::Horizontal, Key::ArrowLeft) => Some(-line),
        (ScrollAxis::Horizontal, Key::ArrowRight) => Some(line),
        (_, Key::PageUp) => Some(-page),
        (_, Key::PageDown) => Some(page),
        (_, Key::Home) => Some(-limit),
        (_, Key::End) => Some(limit),
        _ => None,
    };
    let Some(delta) = goal else {
        return false;
    };
    let handle = ScrollHandle::new(window, node);
    let mut movement = LogicalPoint::new(px(0.0), px(0.0));
    match slot.axis {
        ScrollAxis::Vertical => movement.y = delta,
        ScrollAxis::Horizontal => movement.x = delta,
    }
    app.scroll_by(&handle, movement).is_ok()
}

pub(crate) fn click_count(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    position: LogicalPoint,
    button: PointerButton,
    timestamp: Instant,
) -> u8 {
    let count = match app.last_click {
        Some(last)
            if last.window == window
                && last.node == Some(node)
                && last.button == button
                && timestamp.saturating_duration_since(last.at)
                    <= app.thresholds.double_click_time
                && drag_util::within(
                    last.position,
                    position,
                    app.thresholds.double_click_distance,
                ) =>
        {
            last.count.saturating_add(1)
        }
        _ => 1,
    };
    app.last_click = Some(ClickRecord {
        window,
        node: Some(node),
        button,
        position,
        at: timestamp,
        count,
    });
    count
}

pub(crate) fn on_window_focus_lost(app: &mut App, window: WindowId) {
    app.held_keys.remove(&window);
    app.consumed_keys.remove(&window);
    if app
        .windows
        .get_mut(&window)
        .is_some_and(Window::clear_ime_cursor_request)
    {
        app.dispatcher.wake();
    }
    app.cancel_pointer_session(window);
}

pub(crate) fn cancel_pointer_session(app: &mut App, window: WindowId) {
    let had_capture = app
        .windows
        .get(&window)
        .is_some_and(|target| target.pointer().capture.is_some());
    let cancelled = app
        .windows
        .get_mut(&window)
        .and_then(|target| target.pointer_mut().end_drag());
    if cancelled.is_some() || had_capture {
        app.capture_requests.insert(window, false);
    }
    if let Some(session) = cancelled {
        let delta = session.delta();
        let _ = app.dispatch_action_with_hit(
            window,
            session.node,
            session.action,
            ActionSource::Drag {
                phase: DragPhase::Cancel,
                delta,
            },
            None,
            Modifiers::empty(),
        );
    }
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.press = None;
        pointer.pending_drag = None;
        pointer.cursor = Cursor::Default;
    }
    update_hover(app, window, None);
}

pub(crate) fn hover_chain(
    tree: &crate::node::tree::NodeTree,
    node: Option<NodeId>,
) -> SmallVec<[NodeId; 8]> {
    let mut chain = SmallVec::new();
    let mut cursor = node;
    while let Some(current) = cursor {
        if tree.get(current).is_err() {
            break;
        }
        chain.push(current);
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    chain.reverse();
    chain
}

fn update_hover(app: &mut App, window: WindowId, mut next: Option<NodeId>) {
    let (left, entered) = {
        let Some(target) = app.windows.get(&window) else {
            return;
        };
        let tree = target.tree();
        let scope = overlay_util::focus_scope_root(tree, tree.root());
        next = next.filter(|node| {
            tree.get(*node).is_ok() && overlay_util::in_modal_scope(tree, *node, scope)
        });
        let previous = target
            .pointer()
            .hovered
            .filter(|node| tree.get(*node).is_ok());
        if previous == next {
            return;
        }
        let old = hover_chain(tree, previous);
        let new = hover_chain(tree, next);
        let left: Vec<NodeId> = old
            .iter()
            .rev()
            .filter(|node| !new.contains(node))
            .copied()
            .collect();
        let entered: Vec<NodeId> = new
            .iter()
            .filter(|node| !old.contains(node))
            .copied()
            .collect();
        (left, entered)
    };
    if let Some(target) = app.windows.get_mut(&window) {
        target.pointer_mut().hovered = next;
        let marked: SmallVec<[NodeId; 8]> = left
            .iter()
            .chain(entered.iter())
            .copied()
            .filter(|node| {
                target
                    .tree()
                    .get(*node)
                    .is_ok_and(|slot| slot.has_bindings())
            })
            .collect();
        for node in marked {
            target.tree_mut().mark_bind(node);
            target.tree_mut().mark_paint(node);
        }
    }
    app.events.push(
        window,
        Event::HoverChanged {
            hovered: next,
            left,
            entered,
        },
    );
    app.request_frame();
}

pub(crate) fn revalidate_hover(app: &mut App, window: WindowId) {
    let next = app
        .windows
        .get(&window)
        .and_then(|target| target.pointer().hovered);
    update_hover(app, window, next);
}

pub(crate) fn cleanup_removed_pointer_state(
    app: &mut App,
    window: WindowId,
    removed: &ahash::HashSet<NodeId>,
    removed_hover_chain: Option<SmallVec<[NodeId; 8]>>,
) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    {
        let pointer = target.pointer_mut();
        if pointer
            .press
            .is_some_and(|press| press.node.is_some_and(|node| removed.contains(&node)))
        {
            pointer.press = None;
        }
        if pointer
            .pending_drag
            .as_ref()
            .is_some_and(|pending| removed.contains(&pending.node))
        {
            pointer.pending_drag = None;
        }
        if pointer
            .last_hit
            .is_some_and(|hit| removed.contains(&hit.node))
        {
            pointer.last_hit = None;
        }
    }
    let Some(chain) = removed_hover_chain else {
        return;
    };
    target.pointer_mut().hovered = None;
    let marked: SmallVec<[NodeId; 8]> = chain
        .iter()
        .copied()
        .filter(|node| {
            target
                .tree()
                .get(*node)
                .is_ok_and(|slot| slot.has_bindings())
        })
        .collect();
    for node in marked {
        target.tree_mut().mark_bind(node);
        target.tree_mut().mark_paint(node);
    }
    let left: Vec<NodeId> = chain.iter().rev().copied().collect();
    app.events.push(
        window,
        Event::HoverChanged {
            hovered: None,
            left,
            entered: Vec::new(),
        },
    );
    update_cursor(app, window, None);
    app.request_frame();
}

fn update_cursor(app: &mut App, window: WindowId, hit: Option<HitResult>) {
    let cursor = app.windows.get(&window).map_or(Cursor::Default, |target| {
        let tree = target.tree();
        let scope = overlay_util::focus_scope_root(tree, tree.root());
        let hit = hit.filter(|result| overlay_util::in_modal_scope(tree, result.node, scope));
        focus::resolve_cursor(tree, hit.map(|result| result.node))
    });
    let mut changed = false;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        if pointer.cursor != cursor {
            pointer.cursor = cursor;
            changed = true;
        }
    }
    if changed {
        app.cursor_pending = Some((window, cursor));
    }
}

pub(crate) fn capture_pointer(app: &mut App, window: WindowId, node: NodeId) -> bool {
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    if target.tree().get(node).is_err() {
        return false;
    }
    if target.pointer().capture == Some(node) {
        return false;
    }
    target.pointer_mut().capture = Some(node);
    app.capture_requests.insert(window, true);
    true
}

pub(crate) fn release_pointer(app: &mut App, window: WindowId) -> bool {
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    if target.pointer_mut().capture.take().is_none() {
        return false;
    }
    app.capture_requests.insert(window, false);
    true
}

#[must_use]
pub(crate) fn pointer_captured(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.pointer().capture?;
    target.tree().get(node).is_ok().then_some(node)
}

#[must_use]
pub(crate) fn drag_state(app: &App, window: WindowId) -> Option<DragState> {
    let target = app.windows.get(&window)?;
    let session = target.pointer().drag.as_ref()?;
    Some(DragState::new(window, session.node))
}

#[must_use]
pub(crate) fn drag(app: &App, state: &DragState) -> Option<DragInfo> {
    let target = app.windows.get(&state.window())?;
    let session = target.pointer().drag.as_ref()?;
    if session.node != state.node() {
        return None;
    }
    Some(DragInfo {
        origin: session.origin,
        current: session.current,
        delta: session.delta(),
        phase: session.phase,
    })
}

pub(crate) fn cancel_drag(app: &mut App, state: &DragState) -> bool {
    let window = state.window();
    let session = app
        .windows
        .get_mut(&window)
        .and_then(|target| target.pointer_mut().end_drag());
    let Some(session) = session else {
        return false;
    };
    app.capture_requests.insert(window, false);
    let delta = session.delta();
    let action = session.action;
    let _ = app.dispatch_action_with_hit(
        window,
        session.node,
        action,
        ActionSource::Drag {
            phase: DragPhase::Cancel,
            delta,
        },
        None,
        Modifiers::empty(),
    );
    true
}

pub(crate) fn take_cursor_change(app: &mut App) -> Option<(WindowId, Cursor)> {
    app.cursor_pending.take()
}

pub(crate) fn take_capture_requests(app: &mut App) -> Vec<(WindowId, bool)> {
    std::mem::take(&mut app.capture_requests)
        .into_iter()
        .collect()
}
struct HandlerGuard<'a> {
    app: &'a mut App,
    action: Action,
    handler: Option<Box<dyn FnMut(&mut ActionCtx<'_>)>>,
}

impl Drop for HandlerGuard<'_> {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.app
                .handlers
                .entry(self.action.clone())
                .or_insert(handler);
        }
    }
}

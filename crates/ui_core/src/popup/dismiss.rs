use smallvec::SmallVec;

use crate::{
    app::{App, popup as app_popup},
    math::LogicalPoint,
    popup::{
        DismissOutcome, DismissPolicy, DismissReason, EscapePolicy, OutsidePress, PopupDismiss,
        PopupDismissHandler, PopupError, PopupId, PopupStack, PopupState, PopupTier,
    },
    window::WindowId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PressRouting {
    Dispatch,
    Consume,
}

fn take_handler(app: &mut App, window: WindowId, id: PopupId) -> Option<PopupDismissHandler> {
    app.window_mut(window)
        .and_then(|target| target.popups_mut().take_handler(id))
}

fn restore_handler(app: &mut App, window: WindowId, id: PopupId, handler: PopupDismissHandler) {
    if let Some(target) = app.window_mut(window)
        && target.popups().entry(id).is_ok()
    {
        target.popups_mut().restore_handler(id, handler);
    }
}

pub(crate) fn dispatch(
    app: &mut App,
    window: WindowId,
    id: PopupId,
    reason: DismissReason,
) -> DismissOutcome {
    let handler = take_handler(app, window, id);
    let pointer = app_popup::popup_pointer(app, window);
    let outcome = match handler.as_ref() {
        Some(handle) => handle(
            app,
            PopupDismiss {
                id,
                reason,
                pointer,
            },
        ),
        None => DismissOutcome::Close,
    };
    if outcome == DismissOutcome::Ignore
        && let Some(handler) = handler
    {
        restore_handler(app, window, id, handler);
    }
    outcome
}

pub(crate) fn apply_outcome(
    app: &mut App,
    window: WindowId,
    id: PopupId,
    outcome: DismissOutcome,
) -> bool {
    match outcome {
        DismissOutcome::Ignore => false,
        DismissOutcome::Close => {
            begin_close(app, window, id);
            true
        }
        DismissOutcome::CloseChain => {
            begin_close_chain(app, window, id);
            true
        }
    }
}

fn block_entry(app: &mut App, window: WindowId, id: PopupId) {
    let Some(target) = app.window_mut(window) else {
        return;
    };
    let Ok(entry) = target.popups().entry(id) else {
        return;
    };
    if !matches!(
        entry.state,
        PopupState::Opening | PopupState::Open | PopupState::DismissPending
    ) {
        return;
    }
    let node = entry.node;
    let offset = entry.paint_offset;
    let opacity = target
        .tree()
        .popup_transform(node)
        .map_or(1.0, |transform| transform.opacity);
    let _ = target
        .popups_mut()
        .set_state(id, PopupState::DismissPending);
    target.tree_mut().set_popup_transform(
        node,
        Some(crate::node::tree::PopupTransform {
            offset,
            opacity,
            blocked: true,
        }),
    );
}

pub(crate) fn begin_close(app: &mut App, window: WindowId, id: PopupId) {
    begin_close_inner(app, window, id, false);
}

pub(crate) fn begin_close_chain(app: &mut App, window: WindowId, id: PopupId) {
    begin_close_inner(app, window, id, true);
}

fn begin_close_inner(app: &mut App, window: WindowId, id: PopupId, chain: bool) {
    let Some(target) = app.window(window) else {
        return;
    };
    let stack = target.popups();
    let mut subtree = match stack.subtree(id) {
        Ok(subtree) => subtree,
        Err(_) => return,
    };
    if chain
        && let Some(root) = stack.chain_root(id)
        && let Ok(chain_subtree) = stack.subtree(root)
    {
        subtree = chain_subtree;
    }
    let modal_descendants: SmallVec<[PopupId; 8]> = subtree
        .iter()
        .copied()
        .filter(|current| stack.entry(*current).is_ok_and(|entry| entry.is_modal()))
        .flat_map(|modal| stack.owner_modal_descendants(modal))
        .collect();
    for descendant in modal_descendants {
        if !subtree.contains(&descendant) {
            subtree.push(descendant);
        }
    }
    let ordered: SmallVec<[PopupId; 8]> = subtree.iter().rev().copied().collect();
    let scrims: SmallVec<[PopupId; 2]> = subtree
        .iter()
        .filter_map(|id| {
            stack
                .entry(*id)
                .ok()
                .filter(|entry| entry.is_modal())
                .and_then(|_| stack.scrim_of(*id))
        })
        .collect();
    let mut candidates: SmallVec<[(PopupId, PopupState, crate::popup::PopupAnimation); 10]> =
        SmallVec::with_capacity(ordered.len() + scrims.len());
    for current in scrims.iter().copied() {
        if let Ok(entry) = stack.entry(current) {
            candidates.push((current, entry.state, entry.animation));
        }
    }
    candidates.extend(ordered.iter().filter_map(|current| {
        stack
            .entry(*current)
            .ok()
            .map(|entry| (*current, entry.state, entry.animation))
    }));
    let cascaded: SmallVec<[PopupId; 8]> = candidates
        .iter()
        .filter(|(current, state, _)| {
            *current != id && state.is_dismissable() && !scrims.contains(current)
        })
        .map(|(current, _, _)| *current)
        .collect();
    for descendant in cascaded {
        notify_dismiss(app, window, descendant, DismissReason::ParentClosed);
    }
    let now = app.now();
    let mut animated = false;
    for (current, state, animation) in candidates {
        if matches!(state, PopupState::Closing) {
            continue;
        }
        if state == PopupState::DismissPending {
            animated = true;
            continue;
        }
        block_entry(app, window, current);
        crate::popup::focus::release_focus_on_close(app, window, current);
        if animation == crate::popup::PopupAnimation::None {
            app_popup::close_popup_now(app, current);
            continue;
        }
        animated = true;
        let Some(target) = app.window_mut(window) else {
            return;
        };
        let Ok(entry) = target.popups().entry(current) else {
            continue;
        };
        let node = entry.node;
        let offset = entry.paint_offset;
        let opacity = target
            .tree()
            .popup_transform(node)
            .map_or(1.0, |transform| transform.opacity);
        target
            .popups_mut()
            .push_animation(crate::popup::PopupAnimationState {
                id: current,
                kind: crate::popup::PopupAnimationKind::Closing,
                started: now,
                paint_offset: offset,
                opacity,
            });
    }
    if animated {
        app.request_frame();
    }
}

pub(crate) fn finalize_close(app: &mut App, window: WindowId, id: PopupId) {
    let node = {
        let Some(target) = app.window_mut(window) else {
            return;
        };
        target.popups_mut().remove_animation(id);
        let node = target.popups().entry(id).ok().map(|entry| entry.node);
        if let Some(node) = node {
            target.tree_mut().set_popup_transform(node, None);
        }
        node
    };
    if node.is_none() {
        return;
    }
    app_popup::close_popup_now(app, id);
}

fn outside_consumes(popups: &PopupStack, id: PopupId) -> bool {
    popups.entry(id).is_ok_and(|entry| {
        matches!(
            entry.dismiss,
            DismissPolicy::Chain {
                outside: OutsidePress::Consume,
                ..
            }
        )
    })
}

fn outside_ignores(popups: &PopupStack, id: PopupId) -> bool {
    popups.entry(id).is_ok_and(|entry| {
        matches!(
            entry.dismiss,
            DismissPolicy::Chain {
                outside: OutsidePress::Ignore,
                ..
            }
        )
    })
}

fn resolve_auto_outside(app: &mut App, window: WindowId, top_auto: PopupId) -> PressRouting {
    let (ignores, consumes) = {
        let Some(target) = app.window(window) else {
            return PressRouting::Dispatch;
        };
        (
            outside_ignores(target.popups(), top_auto),
            outside_consumes(target.popups(), top_auto),
        )
    };
    if ignores {
        return PressRouting::Dispatch;
    }
    let outcome = dispatch(app, window, top_auto, DismissReason::OutsidePress);
    match outcome {
        DismissOutcome::Ignore => PressRouting::Dispatch,
        DismissOutcome::Close | DismissOutcome::CloseChain => {
            let _ = apply_outcome(app, window, top_auto, outcome);
            if consumes {
                PressRouting::Consume
            } else {
                PressRouting::Dispatch
            }
        }
    }
}

pub(crate) fn on_pointer_down(
    app: &mut App,
    window: WindowId,
    position: LogicalPoint,
) -> PressRouting {
    let (hit, closing) = {
        let Some(target) = app.window(window) else {
            return PressRouting::Dispatch;
        };
        let stack = target.popups();
        if stack.is_empty() {
            return PressRouting::Dispatch;
        }
        let tree = target.tree();
        let hit = tree.hit_test(position).map(|result| result.node);
        let closing = hit.and_then(|node| stack.opener_chain_closing(tree, node));
        (hit, closing)
    };
    if let Some(closing) = closing
        && cancel_close(app, window, closing)
    {
        return PressRouting::Consume;
    }
    let Some(target) = app.window(window) else {
        return PressRouting::Dispatch;
    };
    let stack = target.popups();
    let tree = target.tree();
    let modal = stack.topmost_modal();
    let hit_enclosing = hit.and_then(|node| stack.enclosing(tree, node));
    let hit_is_scrim = hit_enclosing.is_some_and(|id| {
        stack
            .entry(id)
            .is_ok_and(|entry| entry.tier == PopupTier::Scrim)
    });
    let inside_modal = modal.is_some_and(|modal| {
        !hit_is_scrim && hit.is_some_and(|node| stack.node_in_popup_scope(tree, node, modal))
    });
    if let Some(modal) = modal
        && !inside_modal
    {
        if let Ok(entry) = stack.entry(modal) {
            let explicit_outside = entry.state.is_dismissable()
                && matches!(entry.dismiss, DismissPolicy::Explicit { outside: true, .. });
            if explicit_outside {
                let id = stack.chain_root(modal).unwrap_or(modal);
                let outcome = dispatch(app, window, id, DismissReason::OutsidePress);
                let _ = apply_outcome(app, window, id, outcome);
            }
        }
        return PressRouting::Consume;
    }
    let scope = modal;
    let auto_roots = stack.auto_roots(scope);
    if let Some(top_auto) = auto_roots.last().copied() {
        let inside_any = auto_roots.iter().any(|root| {
            stack.subtree_hit_contains(*root, position)
                || hit.is_some_and(|node| stack.opener_chain(tree, node) == Some(*root))
        });
        if !inside_any {
            return resolve_auto_outside(app, window, top_auto);
        }
    }
    if let Some(node) = hit
        && let Some(chain) = stack.opener_chain(tree, node)
    {
        let toggle = auto_roots.last().copied() == Some(chain);
        if toggle {
            let outcome = dispatch(app, window, chain, DismissReason::Toggle);
            let closed = apply_outcome(app, window, chain, outcome);
            return if closed {
                PressRouting::Consume
            } else {
                PressRouting::Dispatch
            };
        }
        if let Some(open_chain) = auto_roots.last().copied() {
            let outcome = dispatch(app, window, open_chain, DismissReason::Toggle);
            let _ = apply_outcome(app, window, open_chain, outcome);
        }
        return PressRouting::Dispatch;
    }
    if let Some(popup) = stack.popup_at(position) {
        let deepest = stack.subtree(popup).is_ok_and(|subtree| subtree.len() == 1);
        if !deepest {
            let children: SmallVec<[PopupId; 4]> = stack
                .children(popup)
                .into_iter()
                .filter(|child| {
                    stack
                        .entry(*child)
                        .is_ok_and(|entry| entry.state.is_dismissable())
                })
                .collect();
            for child in children.into_iter().rev() {
                begin_close(app, window, child);
            }
        }
        return PressRouting::Dispatch;
    }
    if let Some(top_auto) = auto_roots.last().copied() {
        return resolve_auto_outside(app, window, top_auto);
    }
    PressRouting::Dispatch
}

pub(crate) fn request_close(
    app: &mut App,
    window: WindowId,
    id: PopupId,
    reason: DismissReason,
) -> Result<bool, PopupError> {
    let dismissable = {
        let Some(target) = app.window(window) else {
            return Err(PopupError::UnknownWindow { window });
        };
        target.popups().entry(id)?.state.is_dismissable()
    };
    if !dismissable {
        return Ok(false);
    }
    let outcome = dispatch(app, window, id, reason);
    let still_open = app
        .window(window)
        .is_some_and(|target| target.popups().entry(id).is_ok());
    if !still_open {
        return Ok(true);
    }
    Ok(apply_outcome(app, window, id, outcome))
}

pub(crate) fn escape(app: &mut App, window: WindowId) -> bool {
    let Some(target) = app.window(window) else {
        return false;
    };
    let stack = target.popups();
    if stack.is_empty() {
        return false;
    }
    if let Some(hint) = stack.hint_topmost() {
        let policy = stack
            .entry(hint)
            .map_or(EscapePolicy::CloseTopmost, |entry| entry.dismiss.escape());
        if policy == EscapePolicy::Ignore {
            return false;
        }
        return request_close(app, window, hint, DismissReason::Escape).unwrap_or(false);
    }
    let Some(top) = stack.topmost_dismissable_non_scrim() else {
        return false;
    };
    let policy = stack
        .entry(top)
        .map_or(EscapePolicy::CloseTopmost, |entry| entry.dismiss.escape());
    if policy == EscapePolicy::Ignore {
        return false;
    }
    if policy == EscapePolicy::CloseChain {
        let outcome = dispatch(app, window, top, DismissReason::Escape);
        if outcome == DismissOutcome::Ignore {
            return false;
        }
        if let Some(root) = app
            .window(window)
            .and_then(|target| target.popups().chain_root(top))
        {
            begin_close_chain(app, window, root);
        }
        return true;
    }
    request_close(app, window, top, DismissReason::Escape).unwrap_or(false)
}

pub(crate) fn notify_dismiss(app: &mut App, window: WindowId, id: PopupId, reason: DismissReason) {
    let handler = {
        let Some(target) = app.window_mut(window) else {
            return;
        };
        if !target
            .popups()
            .entry(id)
            .is_ok_and(|entry| entry.state.is_dismissable())
        {
            return;
        }
        target.popups_mut().take_handler(id)
    };
    let Some(handler) = handler else {
        return;
    };
    let pointer = app_popup::popup_pointer(app, window);
    let _ = handler(
        app,
        PopupDismiss {
            id,
            reason,
            pointer,
        },
    );
    if let Some(target) = app.window_mut(window) {
        let stack = target.popups_mut();
        if stack.entry(id).is_ok() {
            stack.restore_handler(id, handler);
        }
    }
}

pub(crate) fn cancel_close(app: &mut App, window: WindowId, id: PopupId) -> bool {
    let Some(target) = app.window_mut(window) else {
        return false;
    };
    let Ok(entry) = target.popups().entry(id) else {
        return false;
    };
    if !matches!(
        entry.state,
        PopupState::Closing | PopupState::DismissPending
    ) {
        return false;
    }
    let node = entry.node;
    target.popups_mut().remove_animation(id);
    target.tree_mut().set_popup_transform(node, None);
    target.popups_mut().mark_dirty();
    crate::popup::animation::start_open(app, window, id);
    crate::popup::focus::take_focus_on_open(app, window, id, node);
    app.request_frame();
    true
}

pub(crate) fn anchor_lost(app: &mut App, window: WindowId, id: PopupId) -> bool {
    let Some(target) = app.window(window) else {
        return false;
    };
    let Some(entry) = target.popups().entry(id).ok() else {
        return false;
    };
    let (dismissable, close_chain) = match entry.dismiss {
        DismissPolicy::Chain { anchor_lost, .. } => (anchor_lost, anchor_lost),
        DismissPolicy::Explicit { .. } => (false, false),
        DismissPolicy::Hint { .. } => (true, false),
    };
    if !dismissable {
        return false;
    }
    if let Some(target) = app.window_mut(window) {
        target
            .popups_mut()
            .push_diagnostic(crate::popup::PopupDiagnostic::AnchorLost { id });
    }
    let outcome = dispatch(app, window, id, DismissReason::AnchorLost);
    if outcome == DismissOutcome::Ignore {
        return false;
    }
    if close_chain {
        if let Some(root) = app
            .window(window)
            .and_then(|target| target.popups().chain_root(id))
        {
            begin_close_chain(app, window, root);
        }
        return true;
    }
    apply_outcome(app, window, id, outcome)
}

pub(crate) fn window_deactivated(app: &mut App, window: WindowId) {
    let Some(target) = app.window(window) else {
        return;
    };
    let stack = target.popups();
    let candidates: SmallVec<[PopupId; 8]> = stack
        .dismissable_entries()
        .into_iter()
        .filter(|id| {
            stack.entry(*id).is_ok_and(|entry| {
                let keeps_on_focus_loss = match entry.dismiss {
                    DismissPolicy::Chain { focus_lost, .. } => !focus_lost,
                    DismissPolicy::Explicit { .. } => true,
                    DismissPolicy::Hint { .. } => false,
                };
                entry.parent.is_none()
                    && !keeps_on_focus_loss
                    && matches!(entry.tier, PopupTier::Auto | PopupTier::Hint)
            })
        })
        .collect();
    for id in candidates.into_iter().rev() {
        let outcome = dispatch(app, window, id, DismissReason::FocusLost);
        let _ = apply_outcome(app, window, id, outcome);
    }
}

use super::{App, RemoveOutcome, input};
use crate::{
    error::EngineError,
    interaction,
    node::{dependency::Owner, id::NodeId, signal::SignalScope},
    window::{Window, WindowId, WindowOptions},
};

pub(crate) fn remove_node(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<RemoveOutcome, EngineError> {
    remove(app, window, node, None)
}

pub(crate) fn remove_node_with_scope(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    scope: SignalScope,
) -> Result<RemoveOutcome, EngineError> {
    remove(app, window, node, Some(scope))
}

fn remove(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    scope: Option<SignalScope>,
) -> Result<RemoveOutcome, EngineError> {
    let (mut outcome, removed_set) = {
        let Some(target) = app.windows.get(&window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        let tree = target.tree();
        if tree.get(node).is_err() || node == tree.root() {
            return Err(EngineError::StaleNode { node });
        }
        let mut ids: Vec<NodeId> = Vec::with_capacity(16);
        tree.for_each_subtree(node, |id| ids.push(id));
        let outcome = RemoveOutcome {
            removed_nodes: ids.len(),
            ..RemoveOutcome::default()
        };
        (outcome, ids)
    };
    let removed_set: ahash::HashSet<NodeId> = removed_set.into_iter().collect();
    #[cfg(debug_assertions)]
    {
        let tree = app.windows.get(&window).expect("① 已校验窗口").tree();
        let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
        while let Some(ancestor) = cursor {
            let removed_row_mounted = tree.virtual_slot(ancestor).is_some_and(|slot| {
                slot.mounted
                    .iter()
                    .any(|(_, row)| removed_set.contains(row))
            });
            debug_assert!(
                !removed_row_mounted,
                "remove_node 删除了虚拟列表的已挂载行：行只能由引擎回收（set_virtual_items / 滚动）。\
                 死 mounted 项会让该条目永不再挂载；请改数据源后调用 set_virtual_items"
            );
            cursor = tree.get(ancestor).ok().and_then(|target| target.parent());
        }
    }
    if let Some(scope) = scope
        && scope != SignalScope::ROOT
        && let Some(reader) = app
            .graph
            .node_reader_outside(app.store.scope_signals(scope), &removed_set)
    {
        return Err(EngineError::ScopeOwnershipMismatch {
            node,
            scope,
            reader,
        });
    }
    let (opener, modal) = app.windows.get(&window).map_or((None, false), |target| {
        let slot = target.tree().overlay_slot(node);
        (
            slot.filter(|slot| slot.restore_focus)
                .and_then(|slot| slot.opener),
            slot.is_some_and(|slot| slot.trap_focus),
        )
    });
    let captured = app.windows.get(&window).is_some_and(|target| {
        let tree = target.tree();
        target
            .pointer()
            .capture
            .is_some_and(|captured| interaction::overlay::is_in_subtree(tree, captured, node))
    });
    if captured {
        app.cancel_pointer_session(window);
        app.events.push(
            window,
            crate::event::Event::Input(crate::event::InputEvent::PointerCancel {
                timestamp: std::time::Instant::now(),
            }),
        );
    }
    let removed_hover_chain = app.windows.get(&window).and_then(|target| {
        let hovered = target.pointer().hovered?;
        removed_set
            .contains(&hovered)
            .then(|| input::hover_chain(target.tree(), Some(hovered)))
    });
    let detached = {
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        target.tree_mut().remove(node)?
    };
    for (removed_node, deps) in detached {
        for signal in deps {
            app.graph.detach(
                signal,
                Owner::Node {
                    window,
                    node: removed_node,
                },
            );
        }
    }
    input::cleanup_removed_pointer_state(app, window, &removed_set, removed_hover_chain);
    let (scoped_key_bindings, timers, animations) =
        cleanup_removed_subtree(app, window, &removed_set);
    outcome.scoped_key_bindings = scoped_key_bindings;
    outcome.timers = timers;
    outcome.animations = animations;
    if modal && let Some(target) = app.windows.get_mut(&window) {
        crate::a11y::tree::mark_modal_scope_changed(target.tree_mut());
    }
    super::focus::restore_focus(app, window, opener);
    if let Some(scope) = scope {
        let animations_before = app.animations.len();
        outcome.released_values = app.release_scope(scope);
        outcome.animations += animations_before - app.animations.len();
    }
    app.request_frame();
    Ok(outcome)
}

pub(crate) fn cleanup_removed_subtree(
    app: &mut App,
    window: WindowId,
    removed: &ahash::HashSet<NodeId>,
) -> (usize, usize, usize) {
    let scoped_key_bindings = {
        let Some(target) = app.windows.get(&window) else {
            return (0, 0, 0);
        };
        let tree = target.tree();
        let before = app.scoped_bindings.len();
        app.scoped_bindings
            .retain(|scoped| scoped.window != window || tree.get(scoped.scope).is_ok());
        before - app.scoped_bindings.len()
    };
    let timers = super::timer::retain_live_scopes(app, window);
    let animations = super::animation::cancel_node_animations(app, window, removed);
    (scoped_key_bindings, timers, animations)
}

pub(crate) fn is_input_blocked_by_modal(app: &App, window: WindowId) -> bool {
    app.windows
        .values()
        .any(|candidate| candidate.options().modal && candidate.options().owner == Some(window))
}

pub(crate) fn create_window(app: &mut App, mut options: WindowOptions) -> WindowId {
    if let Some(owner) = options.owner
        && !app.windows.contains_key(&owner)
    {
        eprintln!(
            "kytrix: window owner {owner:?} does not exist; creating an independent top-level window"
        );
        options.owner = None;
    }
    if options.modal && options.owner.is_none() {
        eprintln!(
            "kytrix: a modal window needs an existing owner; creating a non-modal top-level window"
        );
        options.modal = false;
    }
    let id = WindowId::from_raw(app.next_window_id);
    app.next_window_id = app
        .next_window_id
        .checked_add(1)
        .expect("window handle space exhausted (u32::MAX windows created)");
    let scope = app.store.new_scope();
    app.window_scopes.insert(id, scope);
    let mut window = Window::new(id, options);
    window.tree_mut().bind_window(id);
    app.windows.insert(id, window);
    id
}

#[must_use]
pub(crate) fn window(app: &App, id: WindowId) -> Option<&Window> {
    app.windows.get(&id)
}

pub(crate) fn window_mut(app: &mut App, id: WindowId) -> Option<&mut Window> {
    app.windows.get_mut(&id)
}

pub(crate) fn set_drag_regions(app: &mut App, id: WindowId, regions: &[crate::math::LogicalRect]) {
    let Some(target) = app.windows.get_mut(&id) else {
        return;
    };
    if target.drag_regions() == regions {
        return;
    }
    target.set_drag_regions(regions);
    app.dispatcher.wake();
}

pub(crate) fn on_close(
    app: &mut App,
    id: WindowId,
    handler: impl FnMut(&mut App) -> bool + 'static,
) {
    if !app.windows.contains_key(&id) {
        return;
    }
    app.close_handlers.insert(id, Box::new(handler));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseVerdict {
    Allowed,
    Vetoed,
    Gone,
}

pub(crate) fn can_close_window(app: &mut App, id: WindowId) -> CloseVerdict {
    let Some(mut handler) = app.close_handlers.remove(&id) else {
        return if app.windows.contains_key(&id) {
            CloseVerdict::Allowed
        } else {
            CloseVerdict::Gone
        };
    };
    let allow = handler(app);
    if app.windows.contains_key(&id) && !app.close_handlers.contains_key(&id) {
        app.close_handlers.insert(id, handler);
    }
    if !app.windows.contains_key(&id) {
        return CloseVerdict::Gone;
    }
    if allow {
        CloseVerdict::Allowed
    } else {
        CloseVerdict::Vetoed
    }
}

pub(crate) fn request_close(app: &mut App, id: WindowId) -> bool {
    match can_close_window(app, id) {
        CloseVerdict::Allowed => app.close_window(id),
        CloseVerdict::Vetoed | CloseVerdict::Gone => false,
    }
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn request_close_all(app: &mut App, ids: &[WindowId]) -> bool {
    let mut approved: Vec<WindowId> = Vec::with_capacity(ids.len());
    for &id in ids {
        match can_close_window(app, id) {
            CloseVerdict::Allowed => approved.push(id),
            CloseVerdict::Vetoed => return false,
            CloseVerdict::Gone => {}
        }
    }
    for id in approved {
        app.close_window(id);
    }
    true
}

pub(crate) fn close_window(app: &mut App, id: WindowId) -> bool {
    let owned: Vec<WindowId> = app
        .windows
        .iter()
        .filter(|(_, window)| window.options().owner == Some(id))
        .map(|(window, _)| *window)
        .collect();
    for child in owned {
        close_window(app, child);
    }
    app.close_handlers.remove(&id);
    app.a11y_delivered.remove(&id);
    app.dpi_epochs.remove(&id);
    app.image_dpi_epochs.remove(&id);
    app.scoped_bindings.retain(|scoped| scoped.window != id);
    if app.focused_window == Some(id) {
        app.focused_window = None;
    }
    app.ime_disabled.remove(&id);
    app.animations.retain(|slot| slot.window != id);
    super::timer::cancel_window(app, id);
    super::animation::drop_completions_for_window(app, id);
    app.held_keys.remove(&id);
    app.consumed_keys.remove(&id);
    if app.cursor_pending.is_some_and(|(window, _)| window == id) {
        app.cursor_pending = None;
    }
    if app.windows.contains_key(&id) {
        app.cancel_pointer_session(id);
    }
    app.capture_requests.remove(&id);
    #[cfg(feature = "hud")]
    app.debug_hud.remove(&id);
    app.last_click = app.last_click.filter(|record| record.window != id);
    let Some(window) = app.windows.remove(&id) else {
        return false;
    };
    app.resources.release_window(id);
    if super::resources::invalidate_dead_images(app) > 0 {
        app.request_frame();
    }
    let nodes = window.tree().dfs_ids();
    for node in nodes {
        if let Some(deps) = window.tree().deps(node) {
            for signal in deps {
                app.graph.detach(*signal, Owner::Node { window: id, node });
            }
        }
    }
    if let Some(scope) = app.window_scopes.remove(&id) {
        super::signal_api::release_scope(app, scope);
    }
    true
}

pub(crate) fn request_window_control(
    app: &mut App,
    window: WindowId,
    control: crate::window::WindowControl,
) -> Result<(), EngineError> {
    if !app.windows.contains_key(&window) {
        return Err(EngineError::UnknownWindow { window });
    }
    if let Some(capability) = crate::platform::native::unsupported_window_control(control) {
        return Err(EngineError::UnsupportedWindowCapability { window, capability });
    }
    #[cfg(target_os = "linux")]
    if matches!(
        control,
        crate::window::WindowControl::BeginMove | crate::window::WindowControl::BeginResize(_)
    ) {
        end_local_drag_session(app, window);
    }
    app.window_control = Some((window, control));
    Ok(())
}

#[cfg(target_os = "linux")]
fn end_local_drag_session(app: &mut App, window: WindowId) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    target.pointer_mut().end_drag();
    target.pointer_mut().press = None;
    app.capture_requests.insert(window, false);
}

pub(crate) fn take_window_control(
    app: &mut App,
) -> Option<(WindowId, crate::window::WindowControl)> {
    app.window_control.take()
}

pub(crate) fn set_title(
    app: &mut App,
    window: WindowId,
    title: smol_str::SmolStr,
) -> Result<(), EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    if target.options().title == title {
        return Ok(());
    }
    target.set_title(title);
    app.dispatcher.wake();
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn set_traffic_light_position(
    app: &mut App,
    window: WindowId,
    inset: crate::math::LogicalPoint,
) -> Result<(), EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    if target.traffic_light_position().is_some() {
        return Ok(());
    }
    target.set_traffic_light_position(inset);
    app.dispatcher.wake();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_traffic_light_position(
    app: &mut App,
    window: WindowId,
    _inset: crate::math::LogicalPoint,
) -> Result<(), EngineError> {
    if !app.windows.contains_key(&window) {
        return Err(EngineError::UnknownWindow { window });
    }
    Err(EngineError::UnsupportedWindowCapability {
        window,
        capability: "traffic light position",
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn traffic_light_inset(
    app: &App,
    window: WindowId,
) -> Option<crate::math::LogicalPoint> {
    app.windows.get(&window)?.traffic_light_inset()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn traffic_light_inset(
    _app: &App,
    _window: WindowId,
) -> Option<crate::math::LogicalPoint> {
    None
}

pub(crate) fn windows(app: &App) -> impl Iterator<Item = (WindowId, &Window)> {
    app.windows.iter().map(|(id, window)| (*id, window))
}

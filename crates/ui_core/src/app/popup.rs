use super::App;
use crate::{
    error::EngineError,
    math::{LogicalPoint, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        style::{PaintSpec, Style},
        style_builder::StyleBuilder,
    },
    popup::{
        Anchor, DismissPolicy, DismissReason, FocusPolicy, MAX_POPUP_DEPTH, MAX_POPUP_ENTRIES,
        PlacementInput, PlacementSpec, PopupA11y, PopupDirty, PopupDismissHandler, PopupEntry,
        PopupError, PopupId, PopupRect, PopupSnapshotEntry, PopupSpec, PopupStackSnapshot,
        PopupState, PopupTier, ScrimSpec, Side,
    },
    window::{Window, WindowId},
};

fn popup_content_style() -> Style {
    StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .build()
}

fn scrim_style(viewport: LogicalSize) -> Style {
    StyleBuilder::new()
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .size(viewport.width, viewport.height)
        .build()
}

fn scrim_paint(spec: ScrimSpec) -> PaintSpec {
    PaintSpec {
        background: spec.color.with_opacity(spec.density),
        ..PaintSpec::default()
    }
}

fn empty_rects() -> (LogicalRect, LogicalRect, LogicalRect) {
    (
        LogicalRect::default(),
        LogicalRect::default(),
        LogicalRect::default(),
    )
}

fn effective_placement(parented: bool, placement: &PlacementSpec) -> PlacementSpec {
    if !parented {
        return placement.clone();
    }
    let side = if matches!(placement.side, Side::Left | Side::Right) {
        placement.side
    } else {
        Side::Right
    };
    let mut fallbacks: smallvec::SmallVec<[crate::popup::Placement; 4]> = placement
        .fallbacks
        .iter()
        .copied()
        .filter(|fallback| matches!(fallback.side, Side::Left | Side::Right))
        .collect();
    if fallbacks.is_empty() {
        fallbacks.push(crate::popup::Placement::new(
            Side::Left,
            crate::popup::Alignment::Start,
        ));
    }
    PlacementSpec {
        side,
        fallbacks,
        ..placement.clone()
    }
}

fn viewport_of(app: &App, window: WindowId) -> Result<LogicalSize, PopupError> {
    app.windows
        .get(&window)
        .map(Window::size)
        .ok_or(PopupError::UnknownWindow { window })
}

fn validate_open(
    app: &App,
    window: WindowId,
    spec: &PopupSpec,
) -> Result<(LogicalRect, Option<PopupId>), PopupError> {
    let target = app
        .windows
        .get(&window)
        .ok_or(PopupError::UnknownWindow { window })?;
    let tree = target.tree();
    let popups = target.popups();
    let anchor_rect = crate::popup::anchor::resolve_anchor(tree, spec.anchor)?;
    let mut owner_modal = None;
    if let Some(parent) = spec.parent {
        let chain = popups.chain(parent)?;
        let depth = chain.len() + 1;
        if depth > MAX_POPUP_DEPTH {
            return Err(PopupError::DepthExceeded { depth });
        }
        owner_modal = popups.nearest_modal(Some(parent));
    }
    if owner_modal.is_none()
        && let Some(opener) = spec.opener
        && let Some(enclosing) = popups.enclosing(tree, opener)
    {
        owner_modal = popups.nearest_modal(Some(enclosing));
    }
    Ok((anchor_rect, owner_modal))
}

fn check_limit(app: &App, window: WindowId, additional: usize) -> Result<(), PopupError> {
    let target = app
        .windows
        .get(&window)
        .ok_or(PopupError::UnknownWindow { window })?;
    if target.popups().len() + additional > MAX_POPUP_ENTRIES {
        return Err(PopupError::LimitReached {
            max: MAX_POPUP_ENTRIES,
        });
    }
    Ok(())
}

fn create_content_node(app: &mut App, window: WindowId) -> Result<NodeId, PopupError> {
    let target = app
        .windows
        .get_mut(&window)
        .ok_or(PopupError::UnknownWindow { window })?;
    let (popups, tree) = target.popups_and_tree();
    let root = popups.root();
    NodeBuilder::boxed()
        .style(popup_content_style())
        .id("popup-content")
        .build(tree, root)
        .map_err(|source| PopupError::ContentBuild { node: root, source })
}

fn create_scrim_node(
    app: &mut App,
    window: WindowId,
    viewport: LogicalSize,
    spec: ScrimSpec,
) -> Result<NodeId, PopupError> {
    let target = app
        .windows
        .get_mut(&window)
        .ok_or(PopupError::UnknownWindow { window })?;
    let (popups, tree) = target.popups_and_tree();
    let root = popups.root();
    NodeBuilder::boxed()
        .style(scrim_style(viewport))
        .paint(scrim_paint(spec))
        .id("popup-scrim")
        .build(tree, root)
        .map_err(|source| PopupError::ContentBuild { node: root, source })
}

fn insert_entry(app: &mut App, window: WindowId, entry: PopupEntry) -> Result<PopupId, PopupError> {
    let target = app
        .windows
        .get_mut(&window)
        .ok_or(PopupError::UnknownWindow { window })?;
    target.popups_mut().insert(|id| PopupEntry { id, ..entry })
}

fn cleanup_nodes(app: &mut App, window: WindowId, nodes: &[Option<NodeId>]) {
    for node in nodes.iter().flatten() {
        let _ = app.remove_node(window, *node);
    }
}

fn finalize_open(
    app: &mut App,
    window: WindowId,
    id: PopupId,
    content: NodeId,
) -> Result<(), PopupError> {
    {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        let (popups, tree) = target.popups_and_tree();
        popups
            .sync_order(tree)
            .map_err(|source| PopupError::StackSync { source })?;
        let transparent = popups
            .entry(id)
            .map_err(|_| PopupError::StackSync {
                source: EngineError::StaleNode { node: content },
            })?
            .pointer_transparent;
        tree.set_popup_pointer_transparent(content, transparent);
        popups.mark_dirty();
    }
    crate::popup::animation::start_open(app, window, id);
    crate::popup::focus::take_focus_on_open(app, window, id, content);
    app.request_frame();
    Ok(())
}

pub(crate) fn open_popup(
    app: &mut App,
    window: WindowId,
    spec: PopupSpec,
    content: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>,
    on_dismiss: Option<PopupDismissHandler>,
) -> Result<PopupId, PopupError> {
    let viewport = viewport_of(app, window)?;
    if spec.tier == PopupTier::Scrim {
        return Err(PopupError::InvalidScrim {
            tier: spec.tier,
            focus: spec.focus,
        });
    }
    if spec.scrim.is_some() && (spec.tier != PopupTier::Modal || !spec.focus.is_modal()) {
        return Err(PopupError::InvalidScrim {
            tier: spec.tier,
            focus: spec.focus,
        });
    }
    let (anchor_rect, owner_modal) = validate_open(app, window, &spec)?;
    let placement = effective_placement(spec.parent.is_some(), &spec.placement);
    let _ = crate::popup::placement::solve_placement(PlacementInput {
        anchor: anchor_rect,
        size: LogicalSize::ZERO,
        viewport,
        avoid: None,
        spec: placement.clone(),
    })?;
    let additional = 1 + usize::from(spec.scrim.is_some());
    check_limit(app, window, additional)?;
    if spec.tier == PopupTier::Hint
        && let Some(target) = app.windows.get(&window)
        && let Some(modal) = target.popups().topmost_modal()
        && owner_modal != Some(modal)
    {
        return Err(PopupError::HintBlockedByModal { window });
    }
    let (scrim_z, modal_z) = {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        let stack = target.popups_mut();
        match spec.scrim {
            Some(_) => stack.reserve_z_pair(),
            None => (0, stack.reserve_z()),
        }
    };
    if spec.tier == PopupTier::Modal {
        close_auto_roots_outside(app, window, owner_modal)?;
        close_hints(app, window, owner_modal)?;
    }
    if spec.parent.is_none() && spec.tier == PopupTier::Auto {
        close_auto_roots(app, window)?;
    }
    if let Some(parent) = spec.parent {
        let existing = {
            let target = app
                .windows
                .get(&window)
                .ok_or(PopupError::UnknownWindow { window })?;
            target
                .popups()
                .entry(parent)
                .ok()
                .and_then(|entry| entry.child)
        };
        if let Some(child) = existing {
            close_popup(app, child)?;
        }
    }
    let traps_focus = spec.focus.is_modal();
    let PopupSpec {
        parent,
        tier,
        anchor,
        placement: _spec_placement,
        dismiss,
        focus,
        opener,
        pointer_transparent,
        animation,
        mut a11y,
        scrim,
        ..
    } = spec;
    let content_node = create_content_node(app, window)?;
    if let Err(source) = content(app, window, content_node) {
        let _ = app.remove_node(window, content_node);
        return Err(PopupError::ContentBuild {
            node: content_node,
            source,
        });
    }
    if let Some(active) = a11y.active_descendant {
        let valid = app.windows.get(&window).is_some_and(|target| {
            a11y_active_descendant_in_scope(target.tree(), content_node, &a11y.owns, active)
        });
        if !valid {
            app.a11y_note(crate::a11y::A11yDiagnostic::InvalidActiveDescendant {
                node: content_node,
                target: active,
            });
            a11y.active_descendant = None;
        }
    }
    let scrim_node = match scrim {
        Some(spec) => match create_scrim_node(app, window, viewport, spec) {
            Ok(node) => Some(node),
            Err(error) => {
                let _ = app.remove_node(window, content_node);
                return Err(error);
            }
        },
        None => None,
    };
    let (hit_rect, paint_rect, a11y_rect) = empty_rects();
    let modal_entry = PopupEntry {
        id: PopupId::new(window, 0, 0),
        node: content_node,
        owner_modal,
        parent,
        child: None,
        tier,
        scrim_for: None,
        state: PopupState::Open,
        anchor,
        placement,
        dismiss,
        focus,
        opener,
        pointer_transparent,
        animation,
        a11y,
        z: modal_z,
        hit_rect,
        paint_rect,
        a11y_rect,
        paint_offset: LogicalPoint::ZERO,
        constrained_size: None,
    };
    let modal_id = match insert_entry(app, window, modal_entry) {
        Ok(id) => id,
        Err(error) => {
            cleanup_nodes(app, window, &[Some(content_node), scrim_node]);
            return Err(error);
        }
    };
    if let Some(scrim_node) = scrim_node {
        let (hit_rect, paint_rect, a11y_rect) = empty_rects();
        let scrim_entry = PopupEntry {
            id: PopupId::new(window, 0, 0),
            node: scrim_node,
            owner_modal: None,
            parent: None,
            child: None,
            tier: PopupTier::Scrim,
            scrim_for: Some(modal_id),
            state: PopupState::Open,
            anchor: Anchor::WindowRect(LogicalRect::new(
                px(0.0),
                px(0.0),
                viewport.width,
                viewport.height,
            )),
            placement: PlacementSpec::default(),
            dismiss: DismissPolicy::Hint { escape: false },
            focus: FocusPolicy::None,
            opener: None,
            pointer_transparent: false,
            animation,
            a11y: PopupA11y::default(),
            z: scrim_z,
            hit_rect,
            paint_rect,
            a11y_rect,
            paint_offset: LogicalPoint::ZERO,
            constrained_size: None,
        };
        if let Err(error) = insert_entry(app, window, scrim_entry) {
            let removed = app
                .windows
                .get_mut(&window)
                .and_then(|target| target.popups_mut().remove(modal_id).ok());
            if removed.is_some() {
                let _ = app.remove_node(window, content_node);
            }
            let _ = app.remove_node(window, scrim_node);
            return Err(error);
        }
    }
    if let Some(parent) = parent {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        let _ = target.popups_mut().set_child(parent, Some(modal_id));
    }
    if let Some(handler) = on_dismiss {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        target.popups_mut().set_handler(modal_id, handler);
    }
    if let Err(error) = finalize_open(app, window, modal_id, content_node) {
        let _ = close_popup(app, modal_id);
        return Err(error);
    }
    if traps_focus {
        crate::interaction::scroll::clear_scroll_session(app, window);
    }
    if tier == PopupTier::Modal {
        force_full_paint(app, window);
        app.request_frame();
    }
    Ok(modal_id)
}

fn force_full_paint(app: &mut App, window: WindowId) {
    if let Some(target) = app.windows.get_mut(&window) {
        target.force_full_republish();
    }
}

fn close_auto_roots(app: &mut App, window: WindowId) -> Result<(), PopupError> {
    let roots: Vec<PopupId> = {
        let target = app
            .windows
            .get(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        target
            .popups()
            .order()
            .iter()
            .copied()
            .filter(|id| {
                target
                    .popups()
                    .entry(*id)
                    .is_ok_and(|entry| entry.parent.is_none() && entry.tier == PopupTier::Auto)
            })
            .collect()
    };
    for root in roots {
        crate::popup::dismiss::notify_dismiss(app, window, root, DismissReason::Programmatic);
        close_popup(app, root)?;
    }
    Ok(())
}

fn close_auto_roots_outside(
    app: &mut App,
    window: WindowId,
    owner_modal: Option<PopupId>,
) -> Result<(), PopupError> {
    let roots: Vec<PopupId> = {
        let target = app
            .windows
            .get(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        target
            .popups()
            .order()
            .iter()
            .copied()
            .filter(|id| {
                target.popups().entry(*id).is_ok_and(|entry| {
                    entry.parent.is_none()
                        && entry.tier == PopupTier::Auto
                        && (owner_modal.is_none() || entry.owner_modal != owner_modal)
                })
            })
            .collect()
    };
    for root in roots {
        crate::popup::dismiss::notify_dismiss(app, window, root, DismissReason::Programmatic);
        close_popup(app, root)?;
    }
    Ok(())
}

fn close_hints(
    app: &mut App,
    window: WindowId,
    keep_scope: Option<PopupId>,
) -> Result<(), PopupError> {
    let hints: Vec<PopupId> = {
        let target = app
            .windows
            .get(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        target
            .popups()
            .order()
            .iter()
            .copied()
            .filter(|id| {
                target.popups().entry(*id).is_ok_and(|entry| {
                    entry.tier == PopupTier::Hint
                        && entry.parent.is_none()
                        && (keep_scope.is_none() || entry.owner_modal != keep_scope)
                })
            })
            .collect()
    };
    for hint in hints {
        crate::popup::dismiss::notify_dismiss(app, window, hint, DismissReason::Programmatic);
        close_popup(app, hint)?;
    }
    Ok(())
}

pub(crate) fn close_popup(app: &mut App, id: PopupId) -> Result<(), PopupError> {
    close_subtree(app, id)
}

pub(crate) fn close_popup_chain(app: &mut App, id: PopupId) -> Result<(), PopupError> {
    let window = id.window();
    let chain = {
        let target = app
            .windows
            .get(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        target.popups().chain(id)?
    };
    let root = chain[0];
    close_subtree(app, root)
}

fn close_subtree(app: &mut App, id: PopupId) -> Result<(), PopupError> {
    let window = id.window();
    let subtree = {
        let target = app
            .windows
            .get(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        target.popups().subtree(id)?
    };
    for current in subtree.iter().rev().copied() {
        close_entry(app, current)?;
    }
    app.request_frame();
    Ok(())
}

pub(crate) fn close_popup_now(app: &mut App, id: PopupId) {
    let _ = close_entry(app, id);
}

fn close_entry(app: &mut App, id: PopupId) -> Result<(), PopupError> {
    let window = id.window();
    let Some(target) = app.windows.get(&window) else {
        return Err(PopupError::UnknownWindow { window });
    };
    let entry = target.popups().entry(id)?;
    let node = entry.node;
    let is_scrim = entry.tier == PopupTier::Scrim;
    let scrim_for = entry.scrim_for;
    let is_modal = entry.is_modal();
    let scrim = if is_modal {
        target.popups().scrim_of(id)
    } else {
        None
    };
    let modal_descendants = if is_modal {
        target.popups().owner_modal_descendants(id)
    } else {
        smallvec::SmallVec::new()
    };
    let mut ordered_descendants: smallvec::SmallVec<[(usize, PopupId); 8]> = modal_descendants
        .iter()
        .map(|descendant| {
            (
                target
                    .popups()
                    .chain(*descendant)
                    .map_or(0, |chain| chain.len()),
                *descendant,
            )
        })
        .collect();
    ordered_descendants.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    for (_, descendant) in ordered_descendants {
        crate::popup::dismiss::notify_dismiss(app, window, descendant, DismissReason::ParentClosed);
        let _ = close_entry(app, descendant);
    }
    if is_modal {
        crate::interaction::scroll::clear_scroll_session(app, window);
    }
    crate::popup::focus::release_focus_on_close(app, window, id);
    if is_modal || is_scrim {
        force_full_paint(app, window);
    }
    if let Some(target) = app.windows.get_mut(&window) {
        let _ = target.popups_mut().set_state(id, PopupState::Closed);
    }
    let _ = app.remove_node(window, node);
    let removed = {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        let (popups, tree) = target.popups_and_tree();
        let _ = popups.set_state(id, PopupState::Closed);
        let removed = popups.remove(id)?;
        popups
            .sync_order(tree)
            .map_err(|source| PopupError::StackSync { source })?;
        popups.mark_dirty();
        removed
    };
    if let Some(parent) = removed.parent
        && let Some(target) = app.windows.get_mut(&window)
        && let Ok(parent_entry) = target.popups_mut().entry_mut(parent)
        && parent_entry.child == Some(id)
    {
        parent_entry.child = None;
    }
    if is_scrim && let Some(modal) = scrim_for {
        let still_open = app
            .windows
            .get(&window)
            .is_some_and(|target| target.popups().entry(modal).is_ok());
        if still_open {
            close_entry(app, modal)?;
        }
    }
    if let Some(scrim) = scrim {
        let still_open = app
            .windows
            .get(&window)
            .is_some_and(|target| target.popups().entry(scrim).is_ok());
        if still_open {
            close_entry(app, scrim)?;
        }
    }
    Ok(())
}

pub(crate) fn raise_popup(app: &mut App, id: PopupId) -> Result<(), PopupError> {
    let window = id.window();
    {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        let (popups, tree) = target.popups_and_tree();
        popups.raise(id)?;
        popups
            .sync_order(tree)
            .map_err(|source| PopupError::StackSync { source })?;
        popups.mark_dirty();
    }
    app.request_frame();
    Ok(())
}

pub(crate) fn set_popup_anchor(
    app: &mut App,
    id: PopupId,
    anchor: Anchor,
) -> Result<(), PopupError> {
    let window = id.window();
    {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        let resolved = crate::popup::anchor::resolve_anchor(target.tree(), anchor)?;
        if !resolved.is_convertible() {
            return Err(PopupError::InvalidGeometry {
                anchor: resolved,
                size: LogicalSize::ZERO,
                viewport: target.size(),
                avoid: None,
            });
        }
        target.popups_mut().entry_mut(id)?.anchor = anchor;
        target.popups_mut().mark(PopupDirty {
            geometry: true,
            order: false,
        });
    }
    app.request_frame();
    Ok(())
}

pub(crate) fn request_popup_measure(app: &mut App, id: PopupId) -> Result<(), PopupError> {
    let window = id.window();
    {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        target.popups().entry(id)?;
        target.popups_mut().mark(PopupDirty {
            geometry: true,
            order: false,
        });
    }
    app.request_frame();
    Ok(())
}

pub(crate) fn popup_rect(app: &App, id: PopupId) -> Option<PopupRect> {
    let window = id.window();
    let target = app.windows.get(&window)?;
    let entry = target.popups().entry(id).ok()?;
    let tree = target.tree();
    let bounds = tree.get(entry.node).ok()?.layout().bounds;
    let painted = tree
        .painted_extent(entry.node)
        .map_or(bounds, |extent| extent.layout_bounds());
    Some(PopupRect {
        hit: bounds,
        paint: painted.union(bounds),
        a11y: bounds,
    })
}

pub(crate) fn popup_anchor_rect(app: &App, id: PopupId) -> Option<LogicalRect> {
    let target = app.windows.get(&id.window())?;
    let entry = target.popups().entry(id).ok()?;
    crate::popup::anchor::resolve_anchor(target.tree(), entry.anchor).ok()
}

pub(crate) fn popup_anchor_visible(app: &App, id: PopupId) -> Option<bool> {
    let target = app.windows.get(&id.window())?;
    let entry = target.popups().entry(id).ok()?;
    Some(crate::popup::anchor::anchor_visible(
        target.tree(),
        entry.anchor,
    ))
}

pub(crate) fn popup_stack(app: &App, window: WindowId) -> Option<PopupStackSnapshot> {
    let target = app.windows.get(&window)?;
    let popups = target.popups();
    let mut entries = Vec::with_capacity(popups.len());
    for id in popups.order() {
        let entry = popups.entry(*id).ok()?;
        let rect = popup_rect(app, *id)?;
        entries.push(PopupSnapshotEntry {
            id: *id,
            node: entry.node,
            tier: entry.tier,
            parent: entry.parent,
            z: entry.z,
            state: entry.state,
            hit_rect: rect.hit,
            paint_rect: rect.paint,
            owner_modal: entry.owner_modal,
            focus: entry.focus,
            paint_offset: entry.paint_offset,
            paint_scale: target
                .tree()
                .popup_transform(entry.node)
                .map_or(1.0, crate::node::tree::PopupTransform::paint_scale),
            constrained_size: entry.constrained_size,
        });
    }
    Some(PopupStackSnapshot {
        window,
        root: popups.root(),
        entries,
    })
}

pub(crate) fn popup_pointer(app: &App, window: WindowId) -> Option<LogicalPoint> {
    app.windows.get(&window)?.pointer_position()
}

pub(crate) fn reap_orphan_scrims(app: &mut App, window: WindowId) {
    let previous_effective = crate::app::focus::focused(app, window);
    let owner_before = app
        .windows
        .get(&window)
        .and_then(|target| target.popups().focus_owner());
    let orphans: Vec<PopupId> = {
        let Some(target) = app.windows.get_mut(&window) else {
            return;
        };
        let (popups, tree) = target.popups_and_tree();
        popups.prune_missing(tree);
        popups
            .order()
            .iter()
            .copied()
            .filter(|id| {
                popups.entry(*id).is_ok_and(|entry| {
                    entry.tier == PopupTier::Scrim
                        && tree.get(entry.node).is_ok()
                        && entry
                            .scrim_for
                            .is_none_or(|modal| popups.entry(modal).is_err())
                })
            })
            .collect()
    };
    for id in orphans {
        close_popup_now(app, id);
    }
    let owner_after = app
        .windows
        .get(&window)
        .and_then(|target| target.popups().focus_owner());
    if owner_before.is_some() && owner_after.is_none() {
        let next = crate::app::focus::focused(app, window);
        crate::app::focus::apply_effective_focus(app, window, previous_effective, next);
    }
}

pub(crate) fn begin_close_popup(
    app: &mut App,
    id: PopupId,
    reason: DismissReason,
) -> Result<bool, PopupError> {
    let window = id.window();
    crate::popup::dismiss::request_close(app, window, id, reason)
}

pub(crate) fn set_popup_initial_focus(
    app: &mut App,
    id: PopupId,
    node: NodeId,
) -> Result<bool, PopupError> {
    let window = id.window();
    {
        let target = app
            .windows
            .get_mut(&window)
            .ok_or(PopupError::UnknownWindow { window })?;
        let (popups, tree) = target.popups_and_tree();
        let entry = popups.entry(id)?;
        if !matches!(entry.focus, FocusPolicy::Trap { .. }) || !entry.state.is_dismissable() {
            return Ok(false);
        }
        if tree.get(node).is_err() || !popups.node_in_popup_scope(tree, node, id) {
            return Ok(false);
        }
    }
    Ok(crate::popup::focus::set_owner_focus(
        app,
        window,
        Some(node),
    ))
}

pub(crate) fn popup_state(app: &App, id: PopupId) -> Option<PopupState> {
    let target = app.windows.get(&id.window())?;
    target.popups().entry(id).ok().map(|entry| entry.state)
}

pub(crate) fn set_popup_a11y(
    app: &mut App,
    id: PopupId,
    mut a11y: PopupA11y,
) -> Result<bool, PopupError> {
    let window = id.window();
    let mut diagnostic = None;
    {
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(PopupError::UnknownWindow { window });
        };
        let (popups, tree) = target.popups_and_tree();
        let entry = popups.entry(id)?;
        if entry.a11y == a11y {
            return Ok(false);
        }
        let node = entry.node;
        let invalid = a11y
            .active_descendant
            .filter(|active| !a11y_active_descendant_in_scope(tree, node, &a11y.owns, *active));
        if let Some(active) = invalid {
            diagnostic = Some((node, active));
            a11y.active_descendant = None;
        }
        popups.entry_mut(id)?.a11y = a11y;
        tree.mark_a11y(node);
    }
    if let Some((node, target)) = diagnostic {
        app.a11y_note(crate::a11y::A11yDiagnostic::InvalidActiveDescendant { node, target });
    }
    app.request_frame();
    Ok(true)
}

fn a11y_active_descendant_in_scope(
    tree: &crate::node::tree::NodeTree,
    root: NodeId,
    owns: &[NodeId],
    target: NodeId,
) -> bool {
    tree.get(target).is_ok() && (tree.is_ancestor(root, target) || owns.contains(&target))
}

pub(crate) fn dismiss_topmost(app: &mut App, window: WindowId, reason: DismissReason) -> bool {
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    let Some(id) = target.popups().topmost_dismissable() else {
        return false;
    };
    let Ok(entry) = target.popups().entry(id) else {
        return false;
    };
    if reason == DismissReason::Escape
        && entry.dismiss.escape() == crate::popup::EscapePolicy::Ignore
    {
        return false;
    }
    crate::popup::dismiss::request_close(app, window, id, reason).unwrap_or(false)
}

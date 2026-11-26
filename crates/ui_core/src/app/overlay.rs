use super::{App, input, overlay_style};
use crate::{
    error::EngineError,
    interaction,
    interaction::{
        Overlay, OverlayDismiss, OverlayOptions, OverlayPlacement, OverlaySlot, focus,
        overlay as overlay_util,
    },
    math::{LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    window::WindowId,
};

pub(crate) fn create_overlay(
    app: &mut App,
    window: WindowId,
    options: OverlayOptions,
) -> Result<Overlay, EngineError> {
    let opener = app.windows.get(&window).and_then(|target| {
        let tree = target.tree();
        let scope = overlay_util::focus_scope_root(tree, tree.root());
        let candidate = |node: Option<NodeId>| {
            node.filter(|node| tree.get(*node).is_ok() && focus::in_scope(tree, *node, scope))
        };
        candidate(target.focus().focused())
            .or_else(|| candidate(target.pointer().last_hit.map(|hit| hit.node)))
            .or(scope)
    });
    let OverlayOptions {
        placement,
        dismiss,
        trap_focus,
        restore_focus,
        a11y,
    } = options;
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    let root = target.tree().root();
    let origin = interaction::resolve_placement(target.tree(), placement, LogicalSize::ZERO);
    let style = overlay_style(origin);
    let node = NodeBuilder::boxed()
        .style(style)
        .a11y(a11y)
        .build(target.tree_mut(), root)?;
    if let Some(slot) = target.tree_mut().ensure_interaction(node) {
        slot.overlay = Some(OverlaySlot {
            dismiss,
            placement,
            trap_focus,
            opener,
            restore_focus,
        });
    }
    target.tree_mut().note_overlay_container(node);
    if trap_focus {
        crate::a11y::tree::mark_modal_scope_changed(target.tree_mut());
    }
    if trap_focus {
        input::revalidate_hover(app, window);
        crate::interaction::scroll::clear_scroll_session(app, window);
    }
    #[cfg(feature = "hud")]
    app.raise_debug_hud(window);
    app.request_frame();
    Ok(Overlay::new(window, node))
}

pub(crate) fn close_overlay(app: &mut App, overlay: &Overlay) -> bool {
    app.remove_node(overlay.window(), overlay.node()).is_ok()
}

pub(crate) fn resolve_overlay_placements(app: &mut App, window: WindowId) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    let children: smallvec::SmallVec<[NodeId; 4]> =
        target.tree().overlay_containers().iter().copied().collect();
    let mut moved = false;
    for node in children {
        let Some(slot) = target.tree().overlay_slot(node) else {
            continue;
        };
        if !matches!(slot.placement, OverlayPlacement::Anchor { .. }) {
            continue;
        }
        let size = target
            .tree()
            .get(node)
            .map_or(LogicalSize::ZERO, |target| target.layout().bounds.size);
        let origin = interaction::resolve_placement(target.tree(), slot.placement, size);
        let current = target
            .tree()
            .get(node)
            .map_or(LogicalRect::default(), |target| target.layout().bounds);
        if (current.origin.x - origin.x).abs() < px(0.5)
            && (current.origin.y - origin.y).abs() < px(0.5)
        {
            continue;
        }
        let _ = target.tree_mut().set_absolute_inset(node, origin);
        moved = true;
    }
    if moved {
        app.request_frame();
    }
}

pub(crate) fn place_overlay(app: &mut App, overlay: &Overlay, placement: OverlayPlacement) -> bool {
    let window = overlay.window();
    let node = overlay.node();
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    let size = target
        .tree()
        .get(node)
        .map_or(LogicalSize::ZERO, |target| target.layout().bounds.size);
    let origin = interaction::resolve_placement(target.tree(), placement, size);
    if !target.tree_mut().set_absolute_inset(node, origin) {
        return false;
    }
    if let Some(slot) = target.tree_mut().ensure_interaction(node) {
        slot.overlay = slot.overlay.map(|overlay| OverlaySlot {
            placement,
            ..overlay
        });
    }
    app.request_frame();
    true
}

pub(crate) fn bring_overlay_to_front(app: &mut App, overlay: &Overlay) -> bool {
    let window = overlay.window();
    let node = overlay.node();
    let modal = {
        let Some(target) = app.windows.get_mut(&window) else {
            return false;
        };
        let modal = target
            .tree()
            .overlay_slot(node)
            .is_some_and(|slot| slot.trap_focus);
        let root = target.tree().root();
        if target.tree_mut().append_child(root, node).is_err() {
            return false;
        }
        if modal {
            crate::a11y::tree::mark_modal_scope_changed(target.tree_mut());
        }
        modal
    };
    if modal {
        input::revalidate_hover(app, window);
    }
    #[cfg(feature = "hud")]
    app.raise_debug_hud(window);
    app.request_frame();
    true
}

pub(crate) fn overlays(app: &App, window: WindowId) -> Vec<(NodeId, OverlayDismiss)> {
    let Some(target) = app.windows.get(&window) else {
        return Vec::new();
    };
    let tree = target.tree();
    let Ok(root) = tree.get(tree.root()) else {
        return Vec::new();
    };
    root.children()
        .iter()
        .filter_map(|child| tree.overlay_slot(*child).map(|slot| (*child, slot.dismiss)))
        .collect()
}

pub(crate) fn dismiss_overlay(app: &mut App, window: WindowId) -> bool {
    let node = app.windows.get(&window).and_then(|target| {
        let tree = target.tree();
        overlay_util::topmost(tree, tree.root()).map(|(node, _)| node)
    });
    match node {
        Some(node) => app.remove_node(window, node).is_ok(),
        None => false,
    }
}

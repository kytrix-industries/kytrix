use smallvec::SmallVec;

use super::{AnimationSlot, AnimationTarget, App};
use crate::{
    error::EngineError,
    interaction::{AnimationSpec, ScrollConfig, ScrollHandle, ScrollSlot, ScrollState, scroll},
    math::{LogicalPoint, LogicalPx, LogicalSize},
    node::id::NodeId,
    paint::style::Clip,
    window::WindowId,
};

#[must_use]
pub(crate) fn scroll_handle(app: &App, window: WindowId, node: NodeId) -> Option<ScrollHandle> {
    app.windows
        .get(&window)?
        .tree()
        .scroll_slot(node)
        .map(|_| ScrollHandle::new(window, node))
}

pub(crate) fn make_scroll(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    config: ScrollConfig,
) -> Result<ScrollHandle, EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::NotScrollable { node });
    };
    let slot = target.tree().get(node)?;
    if slot.clip() != Clip::Bounds {
        return Err(EngineError::NotScrollable { node });
    }
    let viewport = scroll::viewport(target.tree(), node).size;
    if !config.line_height.is_finite() || config.line_height <= LogicalPx::ZERO {
        return Err(EngineError::NotScrollable { node });
    }
    if let Some(slot) = target.tree_mut().ensure_interaction(node) {
        slot.scroll = Some(ScrollSlot {
            offset: LogicalPoint::ZERO,
            content: viewport,
            axis: config.axis,
            line_height: config.line_height,
        });
    }
    target.tree_mut().note_scroll_container(node);
    Ok(ScrollHandle::new(window, node))
}

pub(crate) fn set_scroll_content(
    app: &mut App,
    handle: &ScrollHandle,
    content: LogicalSize,
) -> Result<(), EngineError> {
    let window = handle.window();
    let node = handle.node();
    let Some(target) = app.windows.get_mut(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree_mut().scroll_slot_mut(node) else {
        return Err(EngineError::NotScrollable { node });
    };
    if ![content.width, content.height]
        .iter()
        .all(|value| value.is_finite())
    {
        return Err(EngineError::NotScrollable { node });
    }
    slot.content = content;
    Ok(())
}

pub(crate) fn set_scroll_line_height(
    app: &mut App,
    handle: &ScrollHandle,
    line_height: LogicalPx,
) -> Result<(), EngineError> {
    let node = handle.node();
    if !line_height.is_finite() || line_height <= LogicalPx::ZERO {
        return Err(EngineError::NotScrollable { node });
    }
    let window = handle.window();
    let Some(target) = app.windows.get_mut(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree().scroll_slot(node) else {
        return Err(EngineError::NotScrollable { node });
    };
    if slot.line_height == line_height {
        return Ok(());
    }
    if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
        slot.line_height = line_height;
    }
    Ok(())
}

pub(crate) fn scroll_to(
    app: &mut App,
    handle: &ScrollHandle,
    offset: LogicalPoint,
) -> Result<(), EngineError> {
    if ![offset.x, offset.y].iter().all(|value| value.is_finite()) {
        return Err(EngineError::NotScrollable {
            node: handle.node(),
        });
    }
    let window = handle.window();
    let node = handle.node();
    {
        let Some(target) = app.windows.get(&window) else {
            return Ok(());
        };
        if target.tree().get(node).is_err() {
            return Err(EngineError::StaleNode { node });
        }
        if target.tree().scroll_slot(node).is_none() {
            return Err(EngineError::NotScrollable { node });
        }
    }
    super::animation::cancel_scroll_animation(app, handle);
    let Some(target) = app.windows.get_mut(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree().scroll_slot(node) else {
        return Err(EngineError::NotScrollable { node });
    };
    let viewport = scroll::viewport(target.tree(), node);
    let mut probe = slot;
    probe.offset = offset;
    let next = scroll::clamp_offset(&probe, viewport.size);
    if next == slot.offset {
        return Ok(());
    }
    if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
        slot.offset = next;
    }
    let damage = scroll::to_screen(target.tree(), node, viewport);
    target.request_damage(damage);
    target.a11y_note_scroll(node);
    app.request_frame();
    app.rewindow_virtual(window, node);
    Ok(())
}

pub(crate) fn normalize_scroll_offsets(app: &mut App, window: WindowId) {
    if app.clamp_scroll_offsets(window) {
        app.request_frame();
    }
}

pub(crate) fn clamp_scroll_offsets(app: &mut App, window: WindowId) -> bool {
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    let nodes: SmallVec<[crate::node::id::NodeId; 8]> =
        target.tree().scroll_containers().iter().copied().collect();
    let mut changed = false;
    for node in nodes {
        let Some(slot) = target.tree().scroll_slot(node) else {
            continue;
        };
        let viewport = scroll::viewport(target.tree(), node);
        let next = scroll::clamp_offset(&slot, viewport.size);
        if next == slot.offset {
            continue;
        }
        if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
            slot.offset = next;
        }
        let damage = scroll::to_screen(target.tree(), node, viewport);
        target.request_damage(damage);
        target.a11y_note_scroll(node);
        changed = true;
    }
    changed
}

pub(crate) fn scroll_by(
    app: &mut App,
    handle: &ScrollHandle,
    delta: LogicalPoint,
) -> Result<(), EngineError> {
    let current = app
        .scroll_state(handle)
        .map_or(LogicalPoint::ZERO, |state| state.offset);
    app.scroll_to(
        handle,
        LogicalPoint::new(current.x + delta.x, current.y + delta.y),
    )
}

#[must_use]
pub(crate) fn scroll_state(app: &App, handle: &ScrollHandle) -> Option<ScrollState> {
    let target = app.windows.get(&handle.window())?;
    let node = handle.node();
    let slot = target.tree().scroll_slot(node)?;
    Some(ScrollState {
        offset: slot.offset,
        content: slot.content,
        viewport: scroll::viewport(target.tree(), node).size,
        axis: slot.axis,
    })
}

pub(crate) fn scroll_to_animated(
    app: &mut App,
    handle: &ScrollHandle,
    target: LogicalPoint,
) -> Result<(), EngineError> {
    let Some(state) = app.scroll_state(handle) else {
        return Err(EngineError::NotScrollable {
            node: handle.node(),
        });
    };
    let spec = AnimationSpec::short();
    let window = handle.window();
    app.animations.retain(|slot| {
        !matches!(&slot.target, AnimationTarget::Scroll { handle: existing, .. } if *existing == *handle)
    });
    let id = app.next_animation;
    app.next_animation = app.next_animation.wrapping_add(1);
    app.animations.push(AnimationSlot {
        id,
        window,
        target: AnimationTarget::Scroll {
            handle: *handle,
            from: state.offset,
            to: target,
        },
        start: app.clock,
        duration: spec.duration,
        easing: spec.easing,
        nodes: Vec::new(),
    });
    app.request_frame();
    Ok(())
}

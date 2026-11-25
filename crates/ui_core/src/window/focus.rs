use super::{ImeCursorRequest, Window};
use crate::{
    error::EngineError,
    interaction::{FocusScope, scroll},
    math::{LogicalPoint, LogicalRect},
    node::id::NodeId,
};

pub(crate) fn focus(window: &Window) -> &FocusScope {
    &window.focus
}

pub(crate) fn focus_mut(window: &mut Window) -> &mut FocusScope {
    &mut window.focus
}

pub(crate) fn text_caret_rect(
    window: &Window,
    node: NodeId,
    byte_index: usize,
) -> Result<LogicalRect, EngineError> {
    let local = window.tree.text_caret_rect(node, byte_index)?;
    let bounds = window.tree.get(node)?.layout().bounds;
    let content_offset = window.tree.content_offset(node);
    let origin = LogicalPoint::new(
        bounds.origin.x + content_offset.x,
        bounds.origin.y + content_offset.y,
    );
    Ok(scroll::to_screen(
        &window.tree,
        node,
        local.translate(origin),
    ))
}

pub(crate) fn text_byte_index_at(
    window: &Window,
    node: NodeId,
    local_point: LogicalPoint,
) -> Result<usize, EngineError> {
    let content_offset = window.tree.content_offset(node);
    window.tree.text_byte_index_at(
        node,
        LogicalPoint::new(
            local_point.x - content_offset.x,
            local_point.y - content_offset.y,
        ),
    )
}

pub(crate) fn set_ime_cursor_request(window: &mut Window, node: NodeId, byte_index: usize) -> bool {
    let next = ImeCursorRequest { node, byte_index };
    if window.ime_cursor == Some(next) {
        return false;
    }
    window.ime_cursor = Some(next);
    true
}

pub(crate) fn clear_ime_cursor_request(window: &mut Window) -> bool {
    window.ime_cursor.take().is_some()
}

pub(crate) fn ime_cursor_node(window: &Window) -> Option<NodeId> {
    window.ime_cursor.map(|request| request.node)
}

pub(crate) fn ime_cursor_area(window: &Window) -> Result<Option<LogicalRect>, EngineError> {
    let Some(request) = window.ime_cursor else {
        return Ok(None);
    };
    window
        .text_caret_rect(request.node, request.byte_index)
        .map(Some)
}

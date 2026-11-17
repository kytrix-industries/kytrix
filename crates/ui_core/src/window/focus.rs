//! 焦点、指针与文本插入点（含 IME caret hint）。

use super::{ImeCursorRequest, Window};
use crate::{
    error::EngineError,
    interaction::{FocusScope, scroll},
    math::{Point, Rect},
    node::id::NodeId,
};

// 消费者：`impl Window::focus` 的薄委托（外部调用点见该方法文档）。
/// 焦点状态（只读；写入口是 `App::{set_focus, focus_next, focus_prev}`）。
pub(crate) fn focus(window: &Window) -> &FocusScope {
    &window.focus
}

// 消费者：`impl Window::focus_mut` 的薄委托（外部调用点见该方法文档）。
/// 焦点状态的可写引用（写入口是 `App::set_focus`，它负责标脏）。
pub(crate) fn focus_mut(window: &mut Window) -> &mut FocusScope {
    &mut window.focus
}

// 消费者：`impl Window::text_caret_rect` 的薄委托（外部调用点见该方法文档）。
/// 文本插入点的窗口绝对矩形（逻辑像素）。
///
/// `NodeTree::text_caret_rect` 给出文本盒局部矩形；本方法补上节点 border box 原点、
/// 内容盒偏移（border + padding）与祖先滚动偏移。
///
/// # Errors
/// `StaleNode`：节点不存在；`TextShape`：不是已 shape 的文本节点或 `byte_index` 非法。
pub(crate) fn text_caret_rect(
    window: &Window,
    node: NodeId,
    byte_index: usize,
) -> Result<Rect, EngineError> {
    let local = window.tree.text_caret_rect(node, byte_index)?;
    let bounds = window.tree.get(node)?.layout().bounds;
    let content_offset = window.tree.content_offset(node);
    let origin = Point::new(
        bounds.origin.x + content_offset.x,
        bounds.origin.y + content_offset.y,
    );
    Ok(scroll::to_screen(
        &window.tree,
        node,
        local.translate(origin),
    ))
}

// 消费者：`impl Window::set_ime_cursor_request` 的薄委托（外部调用点见该方法文档）。
// ── P16：IME caret hint ────────────────────────────────────────────────────

/// 登记 IME caret hint；返回是否真的变化（同值 no-op）。
pub(crate) fn set_ime_cursor_request(window: &mut Window, node: NodeId, byte_index: usize) -> bool {
    let next = ImeCursorRequest { node, byte_index };
    if window.ime_cursor == Some(next) {
        return false;
    }
    window.ime_cursor = Some(next);
    true
}

// 消费者：`impl Window::clear_ime_cursor_request` 的薄委托（外部调用点见该方法文档）。
/// 清除 IME caret hint；返回是否真的变化。
pub(crate) fn clear_ime_cursor_request(window: &mut Window) -> bool {
    window.ime_cursor.take().is_some()
}

// 消费者：`impl Window::ime_cursor_area` 的薄委托（外部调用点见该方法文档）。
/// 解析当前 IME caret hint（窗口客户区逻辑坐标）。
///
/// `Ok(None)` = 没有 hint；`Err(TextShape)` = 已登记但文本尚未 shape / 文本已变化，
/// 平台层应跳过本次更新并保留上一次有效区域。
pub(crate) fn ime_cursor_area(window: &Window) -> Result<Option<Rect>, EngineError> {
    let Some(request) = window.ime_cursor else {
        return Ok(None);
    };
    window
        .text_caret_rect(request.node, request.byte_index)
        .map(Some)
}

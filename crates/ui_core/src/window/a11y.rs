use super::Window;
use crate::node::id::NodeId;

fn effective_focus(window: &Window) -> Option<NodeId> {
    let focus = window.focus.focused()?;
    if window.tree.get(focus).is_err() {
        return None;
    }
    let scope = crate::interaction::overlay::focus_scope_root(&window.tree, window.tree.root());
    crate::interaction::overlay::in_modal_scope(&window.tree, focus, scope).then_some(focus)
}

pub(crate) fn a11y_pending(window: &Window) -> bool {
    window.a11y.pending(&window.tree, effective_focus(window))
}

pub(crate) fn a11y_focus_pending(window: &Window) -> bool {
    window
        .a11y
        .focus_pending(&window.tree, effective_focus(window))
}

pub(crate) fn a11y_consumer_epoch(window: &Window) -> crate::a11y::adapter::A11yConsumerEpoch {
    window.a11y.consumer_epoch()
}

pub(crate) fn a11y_consumer_epoch_pending(window: &Window) -> bool {
    window.a11y.consumer_epoch_pending()
}

pub(crate) fn a11y_backlog_pending(window: &Window) -> bool {
    window.a11y.backlog()
}

pub(crate) fn a11y_rebuild_pending(window: &Window) -> bool {
    window.a11y.rebuild_pending()
}

pub(crate) fn a11y_engine_of(
    window: &Window,
    id: crate::a11y::A11yNodeId,
) -> Option<crate::node::id::NodeId> {
    window.a11y.engine_of(id)
}

#[cfg(test)]
pub(crate) fn a11y_stable_id(
    window: &Window,
    node: crate::node::id::NodeId,
) -> Option<crate::a11y::A11yNodeId> {
    window.a11y.stable_of(node)
}

pub(crate) fn a11y_force_full(window: &mut Window) {
    window.a11y.force_full();
}

pub(crate) fn a11y_take(
    window: &mut Window,
    diagnostics: &mut Vec<crate::a11y::A11yDiagnostic>,
) -> Option<crate::a11y::A11yTreeSnapshot> {
    let focus = effective_focus(window);
    let Window {
        tree,
        a11y,
        scale,
        options,
        ime_cursor,
        ..
    } = window;
    let caret = ime_cursor.map(|request| (request.node, request.byte_index));
    a11y.take(
        tree,
        focus,
        *scale,
        options.title.as_str(),
        caret,
        diagnostics,
    )
}

pub(crate) fn a11y_note_scroll(window: &mut Window, container: crate::node::id::NodeId) {
    window.a11y.invalidate_subtree(container);
    if let Some(root) = crate::a11y::tree::lifted_scroll_root(&window.tree, container) {
        window.tree.mark_a11y(root);
    }
}

pub(crate) fn a11y_note_binding(window: &mut Window, node: crate::node::id::NodeId) -> bool {
    window.a11y.note_binding_work(node)
}

pub(crate) fn a11y_binding_work(window: &Window) -> bool {
    window.a11y.binding_work()
}

pub(crate) fn a11y_take_bindings(window: &mut Window) -> Vec<crate::node::id::NodeId> {
    window.a11y.take_bindings()
}

pub(crate) fn a11y_recycle_bindings(window: &mut Window, buffer: Vec<crate::node::id::NodeId>) {
    window.a11y.recycle_bindings(buffer);
}

pub(crate) fn a11y_only_frame(window: &Window) -> bool {
    a11y_pending(window)
        && !window.tree.has_layout_work()
        && !window.tree.has_paint_work()
        && !window.tree.has_bind_work()
        && !window.full_damage_pending
        && window.pending_damage.is_none()
        && !window.atlas_rects_pending()
}

#[cfg(feature = "test-support")]
pub(crate) fn a11y_only_frames(window: &Window) -> usize {
    window.a11y.a11y_only_frames()
}

#[cfg(feature = "test-support")]
pub(crate) fn record_a11y_only_frame(window: &mut Window) {
    window.record = Some(crate::window::record::a11y_only_record(window));
}

#[cfg(feature = "test-support")]
pub(crate) fn set_a11y_capture(window: &mut Window, on: bool) {
    window.a11y.set_capture(on);
}

//! Small building blocks shared by the two dialogs.
//!
//! Both dialogs build plain engine subtrees next to kit components, and both must do so
//! without holding a `&mut NodeTree` borrowed from `App` across a kit call (the node tree
//! lives inside the window, which lives inside `App`, so the two borrows cannot overlap).
//! [`build_node`] is the one statement that takes the tree borrow, runs the builder, and
//! gives it back before the next kit call.

use smol_str::format_smolstr;
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, Event, InputEvent},
    node::{id::NodeId, tree::NodeTree},
    window::WindowId,
};

/// Builds one node under `parent` and returns it.
///
/// The tree borrow lives for the duration of `build` only, so the caller can call a kit
/// component that needs `&mut App` in the next statement.
///
/// # Errors
/// The window is gone (`UnknownWindow`), or the builder failed (usually `StaleNode`).
pub(crate) fn build_node(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    build: impl FnOnce(&mut NodeTree, NodeId) -> Result<NodeId, EngineError>,
) -> Result<NodeId, EngineError> {
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    build(tree, parent)
}

/// Removes one node (and its subtree) from the dialog's window.
///
/// A dialog that redraws a region removes the nodes it built last time; the engine's
/// `remove_node` also releases the scoped bindings and timers of that subtree (E16), so a
/// rebuilt region cannot leave handlers behind.
pub(crate) fn remove_node(app: &mut App, window: WindowId, node: NodeId) {
    if let Err(error) = app.remove_node(window, node) {
        log::warn!("removing a rebuilt dialog node failed: {error}");
    }
}

/// Removes the two `Action` handlers an installed input registered.
///
/// `base_ui::kit::input::InputNode::install` registers the click-to-place and drag-to-select
/// handlers under keys derived from the spec name (`"base_ui.input.{name}.click"` /
/// `".drag"`, the kit's documented action convention), and the kit input has no `release`.
/// A dialog that unmounts its input has to take those handlers out itself: each dialog's
/// input name is per-instance unique, so leaving them behind would grow the action table by
/// two entries (and keep the input's state signals alive) for every open/close cycle.
pub(crate) fn release_input_actions(app: &mut App, name: &str) {
    for suffix in [".click", ".drag"] {
        app.off_action(&Action::Custom(format_smolstr!(
            "base_ui.input.{name}{suffix}"
        )));
    }
}

/// Whether `event` is one only a dialog's own text input can consume.
///
/// Typed text, a key press and IME input are what kit's
/// [`InputNode::dispatch`](base_ui::kit::input::InputNode::dispatch) reads while the input
/// has focus. Every other event is the engine's: the platform pushed it and the engine
/// already ran the matching interaction dispatch (hit testing, actions, scrolling), so it
/// is observation for the application rather than a command any dialog may take out of the
/// queue.
pub(crate) fn is_text_input(event: &Event) -> bool {
    matches!(
        event,
        Event::Input(InputEvent::Text { .. } | InputEvent::Key { .. }) | Event::Ime(_)
    )
}

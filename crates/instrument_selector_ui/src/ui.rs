//! Small building blocks shared by the two dialogs.
//!
//! Both dialogs build plain engine subtrees next to kit components, and both must do so
//! without holding a `&mut NodeTree` borrowed from `App` across a kit call (the node tree
//! lives inside the window, which lives inside `App`, so the two borrows cannot overlap).
//! [`build_node`] is the one statement that takes the tree borrow, runs the builder, and
//! gives it back before the next kit call.

use ui_core::{
    app::App,
    error::EngineError,
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

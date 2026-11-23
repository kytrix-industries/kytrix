use super::App;
use crate::{error::EngineError, node::id::NodeId, window::WindowId};

pub(crate) fn request_repaint(app: &mut App, window: WindowId) -> Result<(), EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    target.request_repaint();
    app.request_frame();
    Ok(())
}

pub(crate) fn request_node_repaint(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<bool, EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    let tree = target.tree_mut();
    if tree.get(node).is_err() {
        return Err(EngineError::StaleNode { node });
    }
    if !tree.is_effectively_visible(node) {
        return Ok(false);
    }
    tree.mark_subtree_paint(node);
    app.request_frame();
    Ok(true)
}

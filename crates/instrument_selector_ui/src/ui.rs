use ui_core::{
    app::App,
    error::EngineError,
    event::{Event, InputEvent},
    node::{id::NodeId, tree::NodeTree},
    window::WindowId,
};

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

pub(crate) fn remove_node(app: &mut App, window: WindowId, node: NodeId) {
    if let Err(error) = app.remove_node(window, node) {
        log::warn!("removing a rebuilt dialog node failed: {error}");
    }
}

pub(crate) fn is_text_input(event: &Event) -> bool {
    matches!(
        event,
        Event::Input(InputEvent::Text { .. } | InputEvent::Key { .. }) | Event::Ime(_)
    )
}

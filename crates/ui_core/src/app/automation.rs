use smol_str::SmolStr;

use crate::{
    app::App,
    event::{InputEvent, InputOutcome},
    node::tree::NodeKind,
    window::{FrameIndex, WindowId},
};

pub(crate) fn inject_input(app: &mut App, window: WindowId, event: InputEvent) -> InputOutcome {
    app.dispatch_input(window, event)
}

pub(crate) fn frame_index(app: &App, window: WindowId) -> Option<FrameIndex> {
    app.window(window).map(crate::window::Window::frame_index)
}

pub(crate) fn painted_primitives(app: &App, window: WindowId) -> usize {
    app.window(window)
        .map_or(0, |window| window.scene().primitive_count())
}

pub(crate) fn texts(app: &App, window: WindowId) -> Vec<SmolStr> {
    let Some(window) = app.window(window) else {
        return Vec::new();
    };
    let tree = window.tree();
    let mut texts = Vec::new();
    let mut stack = vec![tree.root()];
    while let Some(node) = stack.pop() {
        let Ok(entry) = tree.get(node) else {
            continue;
        };
        if let NodeKind::Text(content) = entry.kind() {
            texts.push(content.text.clone());
        }
        stack.extend(entry.children().iter().rev().copied());
    }
    texts
}

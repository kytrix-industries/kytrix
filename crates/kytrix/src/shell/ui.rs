//! Node-building helpers shared by the shell's views.
//!
//! The shell builds one persistent node subtree per region and keeps the handles it needs
//! to synchronize it, so these helpers are the small vocabulary every region speaks: a
//! themed box, a themed label, an icon, a separator, and the queue actions are reported
//! through. Nothing here owns state.

use std::{cell::RefCell, rc::Rc};

use base_ui::{
    kit::separator::Separator,
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, role::Role},
};
use smol_str::SmolStr;
use ui_core::{
    app::App,
    error::EngineError,
    interaction::ActionCtx,
    node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    paint::{
        color::Color,
        style::{Style, StylePatch},
    },
    text::TextStyle,
    window::WindowId,
};

use super::intent::{Intent, IntentQueue};

/// The window's node tree.
///
/// # Errors
/// The window is gone.
pub(crate) fn tree(app: &mut App, window: WindowId) -> Result<&mut NodeTree, EngineError> {
    app.window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })
        .map(|target| target.tree_mut())
}

/// A plain container.
pub(crate) fn container(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    style: Style,
) -> Result<NodeId, EngineError> {
    NodeBuilder::boxed()
        .style(style)
        .build(tree(app, window)?, parent)
}

/// A container with a debug id.
pub(crate) fn named(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: &'static str,
    style: Style,
) -> Result<NodeId, EngineError> {
    named_owned(app, window, parent, SmolStr::new_static(id), style)
}

/// A container with a debug id built at run time (a row's workspace name, for instance).
pub(crate) fn named_owned(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: impl Into<SmolStr>,
    style: Style,
) -> Result<NodeId, EngineError> {
    NodeBuilder::boxed()
        .id(id.into())
        .style(style)
        .build(tree(app, window)?, parent)
}

/// A label with a debug id of its own.
pub(crate) fn label_named(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: &'static str,
    text: impl Into<SmolStr>,
    size: f32,
    weight: u16,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let wrapper = named(app, window, parent, id, StyleBuilder::new().build())?;
    label(app, window, wrapper, text, size, weight, role, theme)?;

    Ok(wrapper)
}

/// The fill a role resolves to in the appearance showing now.
pub(crate) fn color(theme: &ThemeHandle, app: &App, role: Role) -> Color {
    theme.theme_for(theme.current(app)).role(role)
}

/// A box painted from a theme role, following appearance changes.
pub(crate) fn themed_box(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    style: Style,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let initial = color(theme, app, role);
    let theme = theme.clone();
    NodeBuilder::boxed()
        .style(style)
        .paint(PaintBuilder::new().background(initial).build())
        .bind_style(move |cx| StylePatch {
            background: Some(theme.role(cx, role)),
            ..StylePatch::default()
        })
        .build(tree(app, window)?, parent)
}

/// A label whose colour follows the appearance.
pub(crate) fn label(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    text: impl Into<SmolStr>,
    size: f32,
    weight: u16,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let initial = color(theme, app, role);
    themed_label(
        tree(app, window)?,
        parent,
        text,
        size,
        weight,
        initial,
        role,
        theme,
    )
}

/// A label on a node tree that is already borrowed (row factories).
pub(crate) fn themed_label(
    tree: &mut NodeTree,
    parent: NodeId,
    text: impl Into<SmolStr>,
    size: f32,
    weight: u16,
    initial: Color,
    role: Role,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(
        text,
        TextStyle::new(size).with_weight(weight).with_color(initial),
    )
    .bind_style(move |cx| StylePatch {
        foreground: Some(theme.role(cx, role)),
        ..StylePatch::default()
    })
    .build(tree, parent)
}

/// A horizontal separator with a themed colour.
pub(crate) fn separator_horizontal(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let tree = tree(app, window)?;
    Separator::horizontal(theme.clone()).build(tree, parent)
}

/// The window's backdrop: the canvas role, painted below everything the window shows.
pub(crate) fn window_backdrop(
    app: &mut App,
    window: WindowId,
    root: NodeId,
    theme: &ThemeHandle,
) -> Result<(), EngineError> {
    let initial = color(theme, app, Role::SurfaceCanvas);
    let themed = theme.clone();
    tree(app, window)?.set_style(
        root,
        StyleBuilder::new()
            .relative()
            .flex_row()
            .size_full()
            .build(),
    )?;
    NodeBuilder::boxed()
        .id(SmolStr::new_static("WINDOW_BACKDROP"))
        .style(
            StyleBuilder::new()
                .absolute()
                .top(0.0)
                .left(0.0)
                .right(0.0)
                .bottom(0.0)
                .build(),
        )
        .paint(PaintBuilder::new().background(initial).build())
        .bind_style(move |cx| StylePatch {
            background: Some(themed.role(cx, Role::SurfaceCanvas)),
            ..StylePatch::default()
        })
        .build(tree(app, window)?, root)?;
    Ok(())
}

/// The intents every control of the shell reports, shared by the views.
pub(crate) type Intents = Rc<RefCell<IntentQueue>>;

/// A fresh intent queue.
pub(crate) fn intents() -> Intents {
    Rc::new(RefCell::new(IntentQueue::default()))
}

/// Reports `intent` and asks for the frame that applies it.
pub(crate) fn report(ctx: &mut ActionCtx<'_>, queue: &Intents, intent: Intent) {
    queue.borrow_mut().push(intent);
    let window = ctx.window();
    let _ = ctx.app_mut().request_repaint(window);
}

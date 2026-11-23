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

pub(crate) fn tree(app: &mut App, window: WindowId) -> Result<&mut NodeTree, EngineError> {
    app.window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })
        .map(|target| target.tree_mut())
}

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

pub(crate) fn named(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: &'static str,
    style: Style,
) -> Result<NodeId, EngineError> {
    named_owned(app, window, parent, SmolStr::new_static(id), style)
}

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

pub(crate) fn color(theme: &ThemeHandle, app: &App, role: Role) -> Color {
    theme.theme_for(theme.current(app)).role(role)
}

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

pub(crate) fn separator_horizontal(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
) -> Result<NodeId, EngineError> {
    let tree = tree(app, window)?;
    Separator::horizontal(theme.clone()).build(tree, parent)
}

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

pub(crate) type Intents = Rc<RefCell<IntentQueue>>;

pub(crate) fn intents() -> Intents {
    Rc::new(RefCell::new(IntentQueue::default()))
}

pub(crate) fn report(ctx: &mut ActionCtx<'_>, queue: &Intents, intent: Intent) {
    queue.borrow_mut().push(intent);
    let window = ctx.window();
    let _ = ctx.app_mut().request_repaint(window);
}

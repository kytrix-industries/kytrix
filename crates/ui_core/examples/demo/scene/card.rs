use smol_str::SmolStr;
use ui_core::{
    app::App,
    math::{Rect, Size},
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, tree::NodeTree},
    paint::{
        primitive::Border,
        style::{PaintSpec, Style},
    },
    text::TextStyle,
    window::WindowId,
};

use super::{CARD_BORDER_RGBA, CARD_RGBA, MUTED_RGBA, color};

const HEADING_SIZE: f32 = 13.5;
const HEADING_WEIGHT: u16 = 700;

pub(crate) fn tree(app: &mut App, window: WindowId) -> &mut NodeTree {
    app.window_mut(window).expect("场景窗口必须存在").tree_mut()
}

pub(crate) fn absolute_style(rect: Rect) -> Style {
    let mut style = Style::default();
    style.inner.position = taffy::style::Position::Absolute;
    style.inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(rect.origin.x),
        top: taffy::style::LengthPercentageAuto::length(rect.origin.y),
        right: taffy::style::LengthPercentageAuto::auto(),
        bottom: taffy::style::LengthPercentageAuto::auto(),
    };
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(rect.size.width),
        height: taffy::style::Dimension::length(rect.size.height),
    };
    style
}

pub(crate) fn flow_style(size: Size) -> Style {
    let mut style = Style::default();
    style.inner.position = taffy::style::Position::Relative;
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(size.width),
        height: taffy::style::Dimension::length(size.height),
    };
    style
}

pub(crate) fn container(app: &mut App, window: WindowId, parent: NodeId, style: Style) -> NodeId {
    NodeBuilder::boxed()
        .style(style)
        .build(tree(app, window), parent)
        .expect("容器挂载")
}

pub(crate) fn card(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    size: Size,
    heading: &str,
    id: &'static str,
) -> NodeId {
    let node = NodeBuilder::boxed()
        .style(flow_style(size))
        .id(id)
        .paint(PaintSpec {
            background: color(CARD_RGBA),
            border: Some(Border {
                width: 1.0,
                color: color(CARD_BORDER_RGBA),
            }),
            corner_radius: [14.0; 4],
            ..PaintSpec::default()
        })
        .build(tree(app, window), parent)
        .expect("卡片挂载");
    let heading_style = TextStyle::new(HEADING_SIZE)
        .with_weight(HEADING_WEIGHT)
        .with_color(color(MUTED_RGBA));
    let _ = label(
        app,
        window,
        node,
        Rect::new(18.0, 12.0, size.width - 36.0, 20.0),
        heading,
        heading_style,
    );
    node
}

pub(crate) fn label(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    rect: Rect,
    text: impl Into<SmolStr>,
    style: TextStyle,
) -> NodeId {
    NodeBuilder::text(text, style)
        .layout_override(rect)
        .build(tree(app, window), parent)
        .expect("文字挂载")
}

pub(crate) fn bound_label(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    rect: Rect,
    style: TextStyle,
    bind: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
) -> NodeId {
    NodeBuilder::text("", style)
        .bind_text(bind)
        .layout_override(rect)
        .build(tree(app, window), parent)
        .expect("动态文字挂载")
}

pub(crate) fn hidden_label(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    rect: Rect,
    text: impl Into<SmolStr>,
    style: TextStyle,
) -> NodeId {
    NodeBuilder::text(text, style)
        .a11y(ui_core::a11y::A11yOverride {
            hidden: true,
            ..ui_core::a11y::A11yOverride::default()
        })
        .layout_override(rect)
        .build(tree(app, window), parent)
        .expect("装饰文字挂载")
}

pub(crate) fn bound_hidden_label(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    rect: Rect,
    style: TextStyle,
    bind: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
) -> NodeId {
    NodeBuilder::text("", style)
        .bind_text(bind)
        .a11y(ui_core::a11y::A11yOverride {
            hidden: true,
            ..ui_core::a11y::A11yOverride::default()
        })
        .layout_override(rect)
        .build(tree(app, window), parent)
        .expect("装饰动态文字挂载")
}

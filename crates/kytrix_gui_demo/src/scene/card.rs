//! 卡片 / 文字 / 绝对定位样式助手（`design/phase-15-demo-showcase.md` §1）。
//!
//! 场景是**绝对定位的卡片网格**：卡片本身用 `Style`（`Position::Absolute` + `inset` + 固定尺寸）
//! 参与 taffy 布局，卡片内的叶子（文字 / 画布 / 色块）用 `LayoutOverride` 相对卡片内容盒定位。
//! 不用 `LayoutOverride` 建卡片：override 节点是布局叶子，`NodeBuilder::build` 的 debug 断言不允许
//! 再挂子节点（`design/phase-04-layout.md` §10）。

use kytrix_gui::{
    app::App,
    math::Rect,
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, tree::NodeTree},
    paint::{
        primitive::Border,
        style::{PaintSpec, Style},
    },
    text::TextStyle,
    window::WindowId,
};
use smol_str::SmolStr;

use super::{CARD_BORDER_RGBA, CARD_RGBA, MUTED_RGBA, color};

/// 卡片标题的字号 / 字重。
const HEADING_SIZE: f32 = 13.5;
/// 卡片标题的字重。
const HEADING_WEIGHT: u16 = 700;

/// 场景窗口的节点树（`window` 必须存在；场景只在有效窗口上建树）。
pub(crate) fn tree(app: &mut App, window: WindowId) -> &mut NodeTree {
    app.window_mut(window).expect("场景窗口必须存在").tree_mut()
}

/// 绝对定位 + 固定尺寸的静态样式（相对父节点 border box）。
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

/// 建一张卡片（背景 + 1px 边框 + 圆角）并返回卡片节点（子节点坐标相对它）。
pub(crate) fn card(
    app: &mut App,
    window: WindowId,
    root: NodeId,
    rect: Rect,
    heading: &str,
) -> NodeId {
    let node = NodeBuilder::boxed()
        .style(absolute_style(rect))
        .paint(PaintSpec {
            background: color(CARD_RGBA),
            border: Some(Border {
                width: 1.0,
                color: color(CARD_BORDER_RGBA),
            }),
            corner_radius: [14.0; 4],
            ..PaintSpec::default()
        })
        .build(tree(app, window), root)
        .expect("卡片挂载");
    let heading_style = TextStyle::new(HEADING_SIZE)
        .with_weight(HEADING_WEIGHT)
        .with_color(color(MUTED_RGBA));
    let _ = label(
        app,
        window,
        node,
        Rect::new(18.0, 12.0, rect.size.width - 36.0, 20.0),
        heading,
        heading_style,
    );
    node
}

/// 静态文字叶子。
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

/// 动态文字叶子（`bind_text` 每次样式 pass 求值）。
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

/// 装饰性文字叶子：**不**出现在无障碍树里（控件的可访问名由容器声明，避免 AT 重复朗读）。
pub(crate) fn hidden_label(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    rect: Rect,
    text: impl Into<SmolStr>,
    style: TextStyle,
) -> NodeId {
    NodeBuilder::text(text, style)
        .a11y(kytrix_gui::a11y::A11yOverride {
            hidden: true,
            ..kytrix_gui::a11y::A11yOverride::default()
        })
        .layout_override(rect)
        .build(tree(app, window), parent)
        .expect("装饰文字挂载")
}

/// 装饰性动态文字叶子（同 [`hidden_label`]，文本由 `bind_text` 求值）。
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
        .a11y(kytrix_gui::a11y::A11yOverride {
            hidden: true,
            ..kytrix_gui::a11y::A11yOverride::default()
        })
        .layout_override(rect)
        .build(tree(app, window), parent)
        .expect("装饰动态文字挂载")
}

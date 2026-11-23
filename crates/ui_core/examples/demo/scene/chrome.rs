use std::time::{Duration, Instant};

use smol_str::SmolStr;
use ui_core::{
    a11y::{A11yAction, A11yOverride, A11yRole},
    app::App,
    event::{Action, ActionSource},
    interaction::DragPhase,
    math::{LogicalPoint, LogicalPx, LogicalRect, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        primitive::Border,
        style::{PaintSpec, Style},
    },
    text::TextAlign,
    window::{WindowControl, WindowId, WindowResizeEdge},
};

use super::{CARD_BORDER_RGBA, CARD_RGBA, MUTED_RGBA, TEXT_RGBA, TITLE, card, color, text_style};

pub(crate) const TITLEBAR_HEIGHT: LogicalPx = px(36.0);

const BUTTON_WIDTH: LogicalPx = px(40.0);

const BUTTON_MARGIN: LogicalPx = px(8.0);

const EDGE_SIZE: LogicalPx = px(6.0);

const CORNER_SIZE: LogicalPx = px(10.0);

const DOUBLE_CLICK: Duration = Duration::from_millis(500);

const DOUBLE_CLICK_DISTANCE: LogicalPx = px(4.0);

#[derive(Clone)]
pub(crate) struct Chrome {
    edges: Vec<(NodeId, WindowResizeEdge)>,
}

pub(crate) fn build(app: &mut App, window: WindowId, root: NodeId) -> Chrome {
    let tree = card::tree(app, window);
    let bar = NodeBuilder::boxed()
        .style(titlebar_style())
        .paint(PaintSpec {
            background: color(CARD_RGBA),
            border: Some(Border {
                width: px(1.0),
                color: color(CARD_BORDER_RGBA),
            }),
            ..PaintSpec::default()
        })
        .build(tree, root)
        .expect("标题栏挂载");

    let drag = NodeBuilder::boxed()
        .style(fill_style())
        .on_drag(Action::custom("chrome.move"))
        .a11y(hidden())
        .build(card::tree(app, window), bar)
        .expect("标题栏拖动区挂载");
    let _ = card::hidden_label(
        app,
        window,
        drag,
        LogicalRect::new(px(12.0), px(9.0), px(560.0), px(18.0)),
        TITLE,
        text_style(px(12.5), 600, MUTED_RGBA),
    );

    let _ = button(
        app,
        window,
        bar,
        BUTTON_MARGIN,
        "×",
        "关闭",
        Action::custom("chrome.close"),
    );
    let _ = button(
        app,
        window,
        bar,
        BUTTON_MARGIN + BUTTON_WIDTH,
        "□",
        "最大化",
        Action::custom("chrome.maximize"),
    );
    let _ = button(
        app,
        window,
        bar,
        BUTTON_MARGIN + BUTTON_WIDTH * 2.0,
        "—",
        "最小化",
        Action::custom("chrome.minimize"),
    );

    let mut edges = Vec::with_capacity(8);
    for (style, edge) in [
        (edge_style(Edge::Top), WindowResizeEdge::Top),
        (edge_style(Edge::Bottom), WindowResizeEdge::Bottom),
        (edge_style(Edge::Left), WindowResizeEdge::Left),
        (edge_style(Edge::Right), WindowResizeEdge::Right),
        (edge_style(Edge::TopLeft), WindowResizeEdge::TopLeft),
        (edge_style(Edge::TopRight), WindowResizeEdge::TopRight),
        (edge_style(Edge::BottomLeft), WindowResizeEdge::BottomLeft),
        (edge_style(Edge::BottomRight), WindowResizeEdge::BottomRight),
    ] {
        let node = NodeBuilder::boxed()
            .style(style)
            .on_drag(Action::custom("chrome.resize"))
            .a11y(hidden())
            .build(card::tree(app, window), root)
            .expect("边框热区挂载");
        edges.push((node, edge));
    }

    Chrome { edges }
}

pub(crate) fn install(app: &mut App, scene: &super::Scene) {
    app.on_action(Action::custom("chrome.close"), |ctx| {
        let window = ctx.window();
        let _ = ctx.app_mut().close_window(window);
    });
    app.on_action(Action::custom("chrome.minimize"), |ctx| {
        let window = ctx.window();
        let _ = ctx
            .app_mut()
            .request_window_control(window, WindowControl::Minimize);
    });
    app.on_action(Action::custom("chrome.maximize"), |ctx| {
        let window = ctx.window();
        let _ = ctx
            .app_mut()
            .request_window_control(window, WindowControl::ToggleMaximize);
    });

    let mut last_press: Option<(Instant, LogicalPoint)> = None;
    app.on_action(Action::custom("chrome.move"), move |ctx| {
        if !matches!(
            ctx.source(),
            ActionSource::Drag {
                phase: DragPhase::Start,
                ..
            }
        ) {
            return;
        }
        let point = ctx.hit().map_or(LogicalPoint::ZERO, |hit| hit.local_point);
        let now = Instant::now();
        let double_click = last_press.is_some_and(|(at, previous)| {
            now.duration_since(at) <= DOUBLE_CLICK
                && (point.x - previous.x).abs() <= DOUBLE_CLICK_DISTANCE
                && (point.y - previous.y).abs() <= DOUBLE_CLICK_DISTANCE
        });
        last_press = if double_click {
            None
        } else {
            Some((now, point))
        };
        let control = if double_click {
            WindowControl::ToggleMaximize
        } else {
            WindowControl::BeginMove
        };
        let window = ctx.window();
        let _ = ctx.app_mut().request_window_control(window, control);
    });

    let edges = scene.chrome.edges.clone();
    app.on_action(Action::custom("chrome.resize"), move |ctx| {
        if !matches!(
            ctx.source(),
            ActionSource::Drag {
                phase: DragPhase::Start,
                ..
            }
        ) {
            return;
        }
        let Some((_, edge)) = edges.iter().find(|(node, _)| *node == ctx.node()) else {
            return;
        };
        let window = ctx.window();
        let _ = ctx
            .app_mut()
            .request_window_control(window, WindowControl::BeginResize(*edge));
    });
}

fn button(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    right: LogicalPx,
    glyph: &'static str,
    label: &'static str,
    action: Action,
) -> NodeId {
    let style = absolute_style(
        None,
        Some(px(0.0)),
        Some(right),
        None,
        taffy::style::Dimension::length(BUTTON_WIDTH.value()),
        taffy::style::Dimension::length(TITLEBAR_HEIGHT.value()),
    );
    let node = NodeBuilder::boxed()
        .style(style)
        .on_click(action)
        .tab_stop(false)
        .a11y(A11yOverride {
            role: Some(A11yRole::Button),
            label: Some(SmolStr::new_static(label)),
            actions: Some(vec![A11yAction::Click]),
            ..A11yOverride::default()
        })
        .build(card::tree(app, window), parent)
        .expect("标题栏按钮挂载");
    let _ = NodeBuilder::text(
        glyph,
        text_style(px(14.0), 400, TEXT_RGBA).with_alignment(TextAlign::Center),
    )
    .layout_override(LogicalRect::new(px(0.0), px(9.0), BUTTON_WIDTH, px(18.0)))
    .a11y(hidden())
    .build(card::tree(app, window), node)
    .expect("标题栏按钮字形挂载");
    node
}

fn titlebar_style() -> Style {
    absolute_style(
        Some(px(0.0)),
        Some(px(0.0)),
        None,
        None,
        taffy::style::Dimension::percent(1.0),
        taffy::style::Dimension::length(TITLEBAR_HEIGHT.value()),
    )
}

fn fill_style() -> Style {
    absolute_style(
        Some(px(0.0)),
        Some(px(0.0)),
        None,
        None,
        taffy::style::Dimension::percent(1.0),
        taffy::style::Dimension::percent(1.0),
    )
}

fn absolute_style(
    left: Option<LogicalPx>,
    top: Option<LogicalPx>,
    right: Option<LogicalPx>,
    bottom: Option<LogicalPx>,
    width: taffy::style::Dimension,
    height: taffy::style::Dimension,
) -> Style {
    use taffy::style::LengthPercentageAuto;

    let length = |value: LogicalPx| LengthPercentageAuto::length(value.value());
    let mut style = Style::default();
    style
        .position(taffy::style::Position::Absolute)
        .width(width)
        .height(height)
        .inset(
            top.map_or_else(LengthPercentageAuto::auto, length),
            right.map_or_else(LengthPercentageAuto::auto, length),
            bottom.map_or_else(LengthPercentageAuto::auto, length),
            left.map_or_else(LengthPercentageAuto::auto, length),
        );
    style
}

#[derive(Clone, Copy)]
enum Edge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

fn edge_style(edge: Edge) -> Style {
    use taffy::style::Dimension;

    let full = Dimension::percent(1.0);
    let edge_size = Dimension::length(EDGE_SIZE.value());
    let corner = Dimension::length(CORNER_SIZE.value());
    let (left, top, right, bottom, width, height) = match edge {
        Edge::Top => (Some(px(0.0)), Some(px(0.0)), None, None, full, edge_size),
        Edge::Bottom => (Some(px(0.0)), None, None, Some(px(0.0)), full, edge_size),
        Edge::Left => (Some(px(0.0)), Some(px(0.0)), None, None, edge_size, full),
        Edge::Right => (None, Some(px(0.0)), Some(px(0.0)), None, edge_size, full),
        Edge::TopLeft => (Some(px(0.0)), Some(px(0.0)), None, None, corner, corner),
        Edge::TopRight => (None, Some(px(0.0)), Some(px(0.0)), None, corner, corner),
        Edge::BottomLeft => (Some(px(0.0)), None, None, Some(px(0.0)), corner, corner),
        Edge::BottomRight => (None, None, Some(px(0.0)), Some(px(0.0)), corner, corner),
    };
    absolute_style(left, top, right, bottom, width, height)
}

fn hidden() -> A11yOverride {
    A11yOverride {
        hidden: true,
        ..A11yOverride::default()
    }
}

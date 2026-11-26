use std::{cell::Cell, rc::Rc};

use super::App;
use crate::{
    error::EngineError,
    event::{
        Action, ActionSource, Event, InputEvent, KeyBinding, KeyCode, KeyState, Modifiers,
        PointerButton, ScrollPhase, ScrollUnit,
    },
    interaction::{
        AnimatedValue, AnimationSpec, DragPhase, Easing, InputThresholds, OverlayDismiss,
        OverlayOptions, OverlayPlacement, ScrollAxis, ScrollConfig, VirtualListConfig,
    },
    math::{DeviceRect, LogicalPoint, LogicalRect, LogicalSize, dp, px},
    node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
    paint::{
        color::Color,
        primitive::Primitive,
        style::{Clip, FocusRing, PaintSpec, Style, StylePatch},
        style_builder::StyleBuilder,
    },
    window::{FrameOutcome, WindowId, WindowOptions},
};

fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    let window = app.create_window(WindowOptions {
        title: "p10".into(),
        size: LogicalSize::new(px(400.0), px(300.0)),
        ..WindowOptions::default()
    });
    (rt, app, window)
}

fn frame(app: &mut App, window: WindowId) -> FrameOutcome {
    app.frame(window)
        .expect("window exists")
        .expect("frame succeeds")
}

#[test]
fn a_deep_layout_change_does_not_damage_the_whole_window() {
    let (_rt, mut app, window) = setup();
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    let outer = NodeBuilder::boxed().build(tree, root).expect("mount");
    let inner = NodeBuilder::boxed().build(tree, outer).expect("mount");
    let label = NodeBuilder::text("100.00", crate::text::TextStyle::new(px(12.0)))
        .build(tree, inner)
        .expect("mount");
    frame(&mut app, window);
    let _ = app.take_wake_request();

    let tree = app.window_mut(window).expect("window").tree_mut();
    tree.set_text_content(
        label,
        crate::text::TextContent::new("1000000.00", crate::text::TextStyle::new(px(12.0))),
    );
    frame(&mut app, window);

    let tree = app.window(window).expect("window").tree();
    assert!(
        tree.get(root).expect("root").damage().is_empty(),
        "根不应因深层布局变化产生 damage（否则整帧重绘）：{:?}",
        tree.get(root).expect("root").damage()
    );
    assert!(
        tree.get(outer).expect("outer").damage().is_empty(),
        "中间容器不应有 damage：{:?}",
        tree.get(outer).expect("outer").damage()
    );
    let _ = inner;
}
fn root_of(app: &App, window: WindowId) -> NodeId {
    app.window(window).expect("window").tree().root()
}

fn background() -> PaintSpec {
    PaintSpec {
        background: Color::from_rgba8([200, 40, 40, 255]),
        ..PaintSpec::default()
    }
}

fn mount_override(app: &mut App, window: WindowId, parent: NodeId, rect: LogicalRect) -> NodeId {
    NodeBuilder::boxed()
        .layout_override(rect)
        .paint(background())
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("mount")
}

fn mount_scroll_container(app: &mut App, window: WindowId, height: f32) -> NodeId {
    let mut style = StyleBuilder::new().size(px(200.0), px(height)).build();
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount")
}

#[test]
fn wheel_events_in_one_frame_accumulate() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("content");

    for _ in 0..2 {
        app.dispatch_input(
            window,
            InputEvent::scroll_lines(
                LogicalPoint::new(px(50.0), px(50.0)),
                LogicalPoint::new(px(0.0), px(-1.0)),
                Modifiers::empty(),
            ),
        );
    }
    app.advance_clock(std::time::Duration::from_millis(400));
    frame(&mut app, window);
    let offset = app
        .window(window)
        .expect("window")
        .tree()
        .scroll_slot(outer)
        .map(|slot| slot.offset.y);
    assert_eq!(offset, Some(px(96.0)), "两次滚轮必须累加（各 48 px）");
}

#[test]
fn non_finite_scroll_inputs_are_rejected() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    assert!(
        app.scroll_to(&handle, LogicalPoint::new(px(f32::NAN), px(0.0)))
            .is_err()
    );
    assert!(
        app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(f32::INFINITY)))
            .is_err()
    );
    assert!(
        app.set_scroll_content(
            &handle,
            crate::math::LogicalSize::new(px(f32::NAN), px(100.0))
        )
        .is_err()
    );
    assert!(
        app.set_scroll_content(
            &handle,
            crate::math::LogicalSize::new(px(100.0), px(f32::INFINITY))
        )
        .is_err()
    );
}

#[test]
fn non_positive_line_height_is_rejected() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    frame(&mut app, window);
    for line_height in [px(0.0), px(-48.0), px(f32::NAN)] {
        let result = app.make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height,
            },
        );
        assert!(result.is_err(), "line_height={line_height} 必须被拒绝");
    }
}
#[test]
fn app_registered_geometry_reaches_the_encoder() {
    let (_rt, mut app, window) = setup();
    let stride = crate::paint::geometry::GeometryPipeline::Ui.vertex_stride() as usize;
    let desc = crate::paint::geometry::GeometryDesc {
        vertex_bytes: bytes::Bytes::from(vec![0u8; stride * 3]),
        index_bytes: bytes::Bytes::from(vec![0u8, 0, 1, 0, 2, 0]),
        vertex_stride: stride as u32,
        index_type: crate::paint::geometry::IndexType::U16,
        topology: crate::paint::geometry::Topology::TriangleList,
        bounds: LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0)),
        pipeline: crate::paint::geometry::GeometryPipeline::Ui,
    };
    let id = app.geometry_mut().insert(desc).expect("valid geometry");
    let root = app.window(window).expect("window").tree().root();
    NodeBuilder::geometry(id)
        .style(StyleBuilder::new().size(px(100.0), px(100.0)).build())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount geometry");
    frame(&mut app, window);

    let draws = app.window(window).expect("window").encoded_geometry();
    let draw = draws.first().expect("必须编码出一条几何绘制");
    assert_eq!(
        draw.snapshot.vertex_len() as usize,
        stride * 3,
        "App 注册的顶点必须真的到达编码器（旧缺陷：编码器持空表 ⇒ 空 desc）"
    );
}

fn lane_quad_bounds(app: &App, window: WindowId, node: NodeId) -> Vec<LogicalRect> {
    let window = app.window(window).expect("window");
    let Some(lane) = window.tree().lane_of(node) else {
        return Vec::new();
    };
    window
        .scene()
        .lane_primitives(lane)
        .unwrap_or(&[])
        .iter()
        .filter_map(|primitive| match primitive {
            Primitive::Quad(quad) => Some(quad.bounds),
            _ => None,
        })
        .collect()
}

#[test]
fn click_dispatches_the_action_and_marks_only_the_target() {
    let (_rt, mut app, window) = setup();
    let calls = Rc::new(Cell::new(0_u32));
    let seen = calls.clone();
    app.on_action(Action::custom("hit"), move |_ctx| seen.set(seen.get() + 1));
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(80.0), px(40.0)))
        .paint(background())
        .on_click(Action::custom("hit"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let tree = app.window_mut(window).expect("window").tree_mut();
    let _ = tree.set_style_patch_for_test(
        node,
        StylePatch {
            interactive: Some(true),
            ..StylePatch::default()
        },
    );
    frame(&mut app, window);
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .clear_all_dirty_for_test();

    let down = app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );

    assert_eq!(calls.get(), 1, "点击必须分发一次");
    assert_eq!(up.action, Some(Action::custom("hit")));
    assert_eq!(up.target, Some(node));
    assert_eq!(up.click_count, 1);
    assert_eq!(down.hit.map(|hit| hit.node), Some(node));
    let tree = app.window(window).expect("window").tree();
    assert!(
        tree.get(node)
            .expect("node")
            .dirty()
            .contains(DirtyFlags::A11Y)
    );
    assert!(
        !tree
            .get(node)
            .expect("node")
            .dirty()
            .contains(DirtyFlags::PAINT)
    );
}

#[test]
fn enter_activates_the_focused_node() {
    let (_rt, mut app, window) = setup();
    let calls = Rc::new(Cell::new(0_u32));
    let seen = calls.clone();
    app.on_action(Action::custom("activate"), move |_ctx| {
        seen.set(seen.get() + 1)
    });
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(50.0), px(50.0)))
        .on_action(Action::custom("activate"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(node)));

    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Enter, Modifiers::empty()),
    );

    assert_eq!(calls.get(), 1);
    assert_eq!(outcome.action, Some(Action::custom("activate")));
    assert_eq!(outcome.target, Some(node));
}

#[test]
fn key_binding_reports_the_key_source() {
    let (_rt, mut app, window) = setup();
    let source = Rc::new(Cell::new(false));
    let seen = source.clone();
    app.on_action(Action::custom("save"), move |ctx| {
        seen.set(matches!(ctx.source(), ActionSource::Key));
    });
    app.bind_key(KeyBinding {
        key: KeyCode::KeyS,
        modifiers: Modifiers::CONTROL,
        action: Action::custom("save"),
    });

    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::KeyS, Modifiers::CONTROL),
    );

    assert!(source.get(), "handler 必须看到 ActionSource::Key");
    assert_eq!(outcome.action, Some(Action::custom("save")));
}

#[test]
fn unhandled_action_is_false_and_engine_actions_need_no_handler() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);

    assert!(!app.dispatch_action(
        window,
        root,
        Action::custom("nobody"),
        ActionSource::Programmatic
    ));
    assert!(app.dispatch_action(window, root, Action::FocusNext, ActionSource::Programmatic));
    assert!(!app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
}

#[test]
fn tab_moves_focus_and_marks_a11y_only() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let first = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("a"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let second = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(60.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("b"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .clear_all_dirty_for_test();

    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Tab, Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(first));
    {
        let tree = app.window(window).expect("window").tree();
        assert!(
            tree.get(first)
                .expect("node")
                .dirty()
                .contains(DirtyFlags::A11Y)
        );
        assert!(
            !tree
                .get(first)
                .expect("node")
                .dirty()
                .contains(DirtyFlags::PAINT),
            "无 ring ⇒ 零像素脏位"
        );
    }

    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Tab, Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(second));
    app.dispatch_input(window, InputEvent::key_down(KeyCode::Tab, Modifiers::SHIFT));
    assert_eq!(app.focused(window), Some(first), "Shift+Tab 回绕");
}

#[test]
fn focus_ring_marks_paint() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("a"))
        .focus_ring(FocusRing::new(
            px(2.0),
            px(2.0),
            Color::from_rgba8([0, 120, 255, 255]),
        ))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .clear_all_dirty_for_test();

    assert!(app.set_focus(window, Some(node)));

    let tree = app.window(window).expect("window").tree();
    let dirty = tree.get(node).expect("node").dirty();
    assert!(dirty.contains(DirtyFlags::PAINT), "有 ring ⇒ PAINT");
    assert!(!dirty.contains(DirtyFlags::REUSABLE));
}

#[test]
fn layout_change_reclamps_the_scroll_offset() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(50.0)))
        .expect("scroll to the bottom");
    frame(&mut app, window);
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .scroll_slot(container)
            .expect("scroll slot")
            .offset
            .y,
        px(50.0),
        "滚到内容末尾"
    );

    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(80.0)))
        .expect("content shrink");
    frame(&mut app, window);
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .scroll_slot(container)
            .expect("scroll slot")
            .offset
            .y,
        px(0.0),
        "内容变短后偏移必须被重新夹紧（旧实现会停在 50）"
    );
}
fn scrollable_app() -> (tokio::runtime::Runtime, App, WindowId, NodeId, NodeId) {
    let (rt, mut app, window) = setup();
    let container = mount_scroll_container(&mut app, window, 150.0);
    let child = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(0.0), px(200.0), px(100.0)),
    );
    let _second = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(100.0), px(200.0), px(100.0)),
    );
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(200.0)))
        .expect("content");
    (rt, app, window, container, child)
}

#[test]
fn text_caret_rect_follows_ancestor_scroll() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let text = NodeBuilder::text("abc", crate::text::TextStyle::new(px(16.0)))
        .layout_override(LogicalRect::new(px(10.0), px(20.0), px(100.0), px(20.0)))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("text");
    frame(&mut app, window);
    let before = app
        .window(window)
        .expect("window")
        .text_caret_rect(text, 3)
        .expect("caret");
    let handle = app.scroll_handle(window, container).expect("scroll");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(50.0)))
        .expect("scroll");
    frame(&mut app, window);
    let after = app
        .window(window)
        .expect("window")
        .text_caret_rect(text, 3)
        .expect("caret");
    assert!((after.origin.y - before.origin.y + 50.0).abs() < px(0.01));
    assert!((after.origin.x - before.origin.x).abs() < px(0.01));
}

#[test]
fn scroll_offset_is_clamped_in_the_same_frame_as_a_resize() {
    let (_rt, mut app, window) = setup();
    let mut style = StyleBuilder::new().size_full().build();
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    let container = NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount");
    app.window_mut(window)
        .expect("window")
        .set_size(crate::math::LogicalSize::new(px(200.0), px(150.0)));
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, crate::math::LogicalSize::new(px(200.0), px(200.0)))
        .expect("content");
    frame(&mut app, window);
    let offset = |app: &App| {
        app.window(window)
            .expect("window")
            .tree()
            .scroll_slot(container)
            .expect("slot")
            .offset
            .y
    };
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(50.0)))
        .expect("scroll");
    frame(&mut app, window);
    assert_eq!(
        offset(&app),
        px(50.0),
        "前置条件：视口 150 / 内容 200 ⇒ max 50"
    );

    app.window_mut(window)
        .expect("window")
        .set_size(crate::math::LogicalSize::new(px(200.0), px(400.0)));
    frame(&mut app, window);
    assert_eq!(offset(&app), px(0.0), "resize 那一帧就必须夹紧");
}

#[cfg(feature = "test-support")]
#[test]
fn a_one_pixel_scroll_frame_rebuilds_nothing_and_damages_only_the_viewport() {
    let (_rt, mut app, window, container, child) = scrollable_app();
    let root = app.window(window).expect("window").tree().root();
    let fixed = mount_override(
        &mut app,
        window,
        root,
        LogicalRect::new(px(300.0), px(0.0), px(40.0), px(40.0)),
    );
    let _ = app
        .window_mut(window)
        .expect("window")
        .tree_mut()
        .mark_paint(container);
    frame(&mut app, window);
    let slots = app.window(window).expect("window").scroll_slots().to_vec();
    assert_eq!(slots.len(), 2, "哨兵 + 一个滚动容器");
    let viewport = slots[1].1;
    let fixed_before = lane_quad_bounds(&app, window, fixed);
    let child_before = lane_quad_bounds(&app, window, child);
    assert!(!child_before.is_empty() && !fixed_before.is_empty());

    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(1.0)))
        .expect("scroll 1px");
    frame(&mut app, window);

    let record = app
        .window(window)
        .expect("window")
        .record()
        .expect("滚动帧必须提交并留下记录");
    assert_eq!(
        record.stats.encoded_lanes, 0,
        "滚动帧不得重编码任何 lane（偏移在绘制期由 GPU 加上）"
    );
    assert!(
        record.dirty_nodes.is_empty(),
        "滚动帧不得重建任何 lane（实际重建 {})",
        record.dirty_nodes.len()
    );
    assert!(
        record.paint_log.lanes.is_empty(),
        "纯滚动帧必须走快路径（不遍历树）：paint 日志应为空，实际 {} 条",
        record.paint_log.lanes.len()
    );
    assert!(!record.damage_full, "滚动只脏视口，不该退化成整帧");
    let padded = crate::math::DeviceRect::new(
        viewport.x0 - 1,
        viewport.y0 - 1,
        viewport.x1 + 1,
        viewport.y1 + 1,
    );
    assert_eq!(
        record.damage,
        vec![padded],
        "damage 必须恰好是滚动容器的视口（物理像素 + 1 px AA 外扩）"
    );
    assert_eq!(
        lane_quad_bounds(&app, window, child),
        child_before,
        "滚动不得改容器内容的图元（偏移在槽位表里）"
    );
    assert_eq!(
        lane_quad_bounds(&app, window, fixed),
        fixed_before,
        "滚动容器之外的固定元素不受偏移影响"
    );
    assert_eq!(
        app.window(window).expect("window").scroll_slots()[1].0,
        LogicalPoint::new(px(0.0), px(1.0)),
        "偏移进槽位表"
    );
    let view = app.window(window).expect("window");
    let lane = view.tree().lane_of(child).expect("child lane");
    assert_eq!(
        view.scene().lane_scroll(lane),
        1,
        "滚动容器内的 lane 必须登记槽位 1"
    );
}

fn mount_nested_scroll_container(app: &mut App, window: WindowId, outer: NodeId) -> NodeId {
    let mut style = StyleBuilder::new()
        .size(px(200.0), px(100.0))
        .margin_sides(px(300.0), px(0.0), px(0.0), px(0.0))
        .build();
    style.clip = Clip::Bounds;
    NodeBuilder::boxed()
        .style(style)
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("mount inner")
}

fn scroll_slot_of(app: &App, window: WindowId, node: NodeId) -> Option<(LogicalPoint, DeviceRect)> {
    let view = app.window(window).expect("window");
    let slot = view.tree().scroll_slot_cached(node) as usize;
    view.scroll_slots().get(slot).copied()
}

fn register_slots(app: &mut App, window: WindowId, nodes: [NodeId; 2]) {
    for node in nodes {
        let _ = app
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(node);
    }
    frame(app, window);
}

#[test]
fn a_nested_scroll_container_follows_the_outer_container_scroll() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    let inner = mount_nested_scroll_container(&mut app, window, outer);
    let row = mount_override(
        &mut app,
        window,
        inner,
        LogicalRect::new(px(0.0), px(0.0), px(200.0), px(40.0)),
    );
    frame(&mut app, window);
    let outer_handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("outer scroll");
    app.set_scroll_content(&outer_handle, LogicalSize::new(px(200.0), px(800.0)))
        .expect("outer content");
    let inner_handle = app
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("inner scroll");
    app.set_scroll_content(&inner_handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("inner content");
    register_slots(&mut app, window, [outer, inner]);
    assert!(
        scroll_slot_of(&app, window, inner).is_some(),
        "前置条件：内层滚动容器必须已登记槽位"
    );

    app.scroll_to(&outer_handle, LogicalPoint::new(px(0.0), px(320.0)))
        .expect("outer scroll");
    frame(&mut app, window);

    let (offset, viewport) = scroll_slot_of(&app, window, inner).expect("inner slot");
    assert_eq!(
        offset,
        LogicalPoint::new(px(0.0), px(0.0)),
        "内层自身未滚动"
    );
    assert_eq!(
        viewport,
        DeviceRect::new(dp(0), dp(0), dp(200), dp(80)),
        "内层槽位的屏幕裁剪区必须随外层滚动重算（旧裁剪区 = 首帧的空交集）"
    );
    assert_eq!(
        lane_quad_bounds(&app, window, row),
        vec![LogicalRect::new(px(0.0), px(-20.0), px(200.0), px(40.0))],
        "内层行的烘焙偏移必须含外层位移（否则它停在旧坐标、被旧裁剪区剪掉）"
    );
}

#[test]
fn a_virtual_list_inside_a_page_scroll_stays_visible_after_scrolling_the_page() {
    let (_rt, mut app, window) = setup();
    let page = mount_scroll_container(&mut app, window, 150.0);
    let inner = mount_nested_scroll_container(&mut app, window, page);
    frame(&mut app, window);
    {
        let mut style = app
            .window(window)
            .expect("window")
            .tree()
            .get(inner)
            .expect("inner")
            .style()
            .clone();
        style.position(taffy::style::Position::Relative);
        let _ = app
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style_for_test(inner, style);
    }
    let list = app
        .make_virtual_list(
            window,
            inner,
            VirtualListConfig {
                extent: px(25.0),
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |index| {
        NodeBuilder::boxed()
            .paint(background())
            .id(format!("row{index}"))
    })
    .expect("items");
    let page_handle = app
        .make_scroll(
            window,
            page,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("page scroll");
    app.set_scroll_content(&page_handle, LogicalSize::new(px(200.0), px(800.0)))
        .expect("page content");
    register_slots(&mut app, window, [page, inner]);
    let rows = app
        .window(window)
        .expect("window")
        .tree()
        .virtual_slot(inner)
        .expect("virtual slot")
        .mounted
        .clone();
    assert!(!rows.is_empty(), "前置条件：虚拟列表已挂载可见行");

    app.scroll_to(&page_handle, LogicalPoint::new(px(0.0), px(320.0)))
        .expect("page scroll");
    frame(&mut app, window);

    let still_mounted = app
        .window(window)
        .expect("window")
        .tree()
        .virtual_slot(inner)
        .expect("virtual slot")
        .mounted
        .len();
    assert_eq!(still_mounted, rows.len(), "外层滚动不得丢行");
    let (_, viewport) = scroll_slot_of(&app, window, inner).expect("inner slot");
    assert_eq!(
        viewport,
        DeviceRect::new(dp(0), dp(0), dp(200), dp(80)),
        "虚拟列表槽位的屏幕裁剪区必须随页面滚动重算"
    );
    let first_row = rows
        .iter()
        .find(|(index, _)| *index == 0)
        .map(|(_, row)| *row)
        .expect("第 0 行已挂载");
    assert_eq!(
        lane_quad_bounds(&app, window, first_row)[0],
        LogicalRect::new(px(0.0), px(-20.0), px(200.0), px(25.0)),
        "行图元的烘焙偏移必须含页面位移"
    );
}

#[cfg(all(windows, feature = "test-support"))]
#[test]
fn an_idle_frame_does_not_advance_the_inline_composer() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let third = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(900.0), px(200.0), px(100.0)),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(1100.0)))
        .expect("content");
    frame(&mut app, window);

    let patch = StylePatch {
        background: Some(Color::BLACK),
        ..StylePatch::default()
    };
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style_patch(third, patch)
    );
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(third)
    );
    let idle = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
            None,
            None,
            None,
        )
        .expect("idle paint");
    assert_eq!(
        (idle.damage.rects, idle.damage.full),
        (0, false),
        "前置条件：视口外改色必须没有像素 damage（lane 可以重建，但不会提交）"
    );
    assert!(
        !app.window(window).expect("window").has_pending_payload(),
        "Idle 帧不能提前合成出已被丢弃的 inline payload"
    );
    assert!(
        app.window(window)
            .expect("window")
            .pending_delta()
            .is_some(),
        "Idle 帧仍必须保留 SceneDelta"
    );

    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(900.0)))
        .expect("scroll");
    let scroll = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
            None,
            None,
            None,
        )
        .expect("scroll paint");
    assert!(
        scroll.uploaded_bytes > 0,
        "滚动提交帧必须重新编码出 lane 实例字节，不能沿用 Idle 帧提前推进的编码器状态"
    );
    assert!(
        app.window(window).expect("window").has_pending_payload(),
        "滚动帧必须产出可发布的 inline payload"
    );
}

#[cfg(all(windows, feature = "test-support"))]
#[test]
fn consecutive_idle_frames_accumulate_their_scene_deltas() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let third = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(900.0), px(200.0), px(100.0)),
    );
    let fourth = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(1000.0), px(200.0), px(100.0)),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(1100.0)))
        .expect("content");
    frame(&mut app, window);
    app.window_mut(window).expect("window").detach_composer();

    let mut lanes = Vec::new();
    for node in [third, fourth] {
        let patch = StylePatch {
            background: Some(Color::BLACK),
            ..StylePatch::default()
        };
        assert!(
            app.window_mut(window)
                .expect("window")
                .tree_mut()
                .set_style_patch(node, patch)
        );
        assert!(
            app.window_mut(window)
                .expect("window")
                .tree_mut()
                .mark_paint(node)
        );
        let report = app
            .window_mut(window)
            .expect("window")
            .paint_frame(
                &crate::paint::geometry_store::GeometryStore::new(),
                None,
                None,
                None,
            )
            .expect("idle paint");
        assert_eq!(
            (report.damage.rects, report.damage.full),
            (0, false),
            "视口外改色必须没有像素 damage"
        );
        lanes.push(
            app.window(window)
                .expect("window")
                .tree()
                .lane_of(node)
                .expect("lane"),
        );
    }

    let pending = app
        .window(window)
        .expect("window")
        .pending_delta()
        .expect("连续 Idle 帧必须保留 delta");
    for lane in &lanes {
        assert!(
            pending.lanes.iter().any(|delta| delta.lane == *lane),
            "连续 Idle 帧的 delta 必须累积，不能只保留最后一帧"
        );
    }
    assert_eq!(
        pending
            .lanes
            .iter()
            .filter(|delta| delta.lane == lanes[0])
            .count(),
        1,
        "同一 lane 只能出现一次"
    );

    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(900.0)))
        .expect("scroll");
    let report = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
            None,
            None,
            None,
        )
        .expect("scroll paint");
    assert!(report.damage.rects > 0, "滚动帧必须产出 damage");
    let pending = app
        .window(window)
        .expect("window")
        .pending_delta()
        .expect("滚动帧 delta");
    for lane in &lanes {
        assert!(
            pending.lanes.iter().any(|delta| delta.lane == *lane),
            "提交帧必须带上前几帧保留的全部 lane"
        );
    }
}

#[cfg(all(windows, feature = "test-support"))]
#[test]
fn a_removed_lane_survives_an_idle_frame_and_reaches_the_next_commit() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let third = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(900.0), px(200.0), px(100.0)),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(1100.0)))
        .expect("content");
    frame(&mut app, window);
    app.window_mut(window).expect("window").detach_composer();

    let patch = StylePatch {
        background: Some(Color::BLACK),
        ..StylePatch::default()
    };
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style_patch(third, patch)
    );
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(third)
    );
    let idle = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
            None,
            None,
            None,
        )
        .expect("idle paint");
    assert_eq!((idle.damage.rects, idle.damage.full), (0, false));
    let lane = app
        .window(window)
        .expect("window")
        .tree()
        .lane_of(third)
        .expect("third lane");
    assert!(
        app.window(window)
            .expect("window")
            .pending_delta()
            .expect("idle delta")
            .lanes
            .iter()
            .any(|delta| delta.lane == lane),
        "前置条件：Idle 帧保留了这个 lane 的新图元"
    );

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(third)
        .expect("remove");
    let _ = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
            None,
            None,
            None,
        )
        .expect("removal paint");
    let pending = app
        .window(window)
        .expect("window")
        .pending_delta()
        .expect("removal delta");
    assert!(
        pending.removed.contains(&lane),
        "被移除的 lane 必须留在 delta 的 removed 里，等下一次提交"
    );
    assert!(
        !pending
            .lanes
            .iter()
            .any(|delta| delta.lane.slot() == lane.slot()),
        "同槽的旧代际 lane 不得与 removed 同时存在（否则会复活）"
    );
}

#[cfg(all(windows, feature = "test-support"))]
#[test]
fn an_a11y_only_frame_still_runs_after_a_retained_delta() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let third = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(900.0), px(200.0), px(100.0)),
    );
    let label_signal = app.signal(smol_str::SmolStr::new_static("旧的"));
    let source = label_signal;
    let label = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(20.0)))
        .paint(background())
        .bind_a11y(move |cx| crate::a11y::A11yPatch {
            label: Some(cx.read(&source).clone()),
            ..crate::a11y::A11yPatch::default()
        })
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("a11y label");
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(1100.0)))
        .expect("content");
    frame(&mut app, window);
    let _ = app.a11y_take(window);
    app.window_mut(window).expect("window").detach_composer();

    let patch = StylePatch {
        background: Some(Color::BLACK),
        ..StylePatch::default()
    };
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style_patch(third, patch)
    );
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(third)
    );
    let idle = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
            None,
            None,
            None,
        )
        .expect("idle paint");
    assert_eq!((idle.damage.rects, idle.damage.full), (0, false));
    assert!(
        app.window(window)
            .expect("window")
            .pending_delta()
            .is_some(),
        "前置条件：Idle 帧必须保留 delta"
    );

    assert!(
        app.write(&label_signal, smol_str::SmolStr::new_static("新的")),
        "写 a11y 绑定必须失效"
    );
    let outcome = frame(&mut app, window);
    assert!(
        matches!(outcome, FrameOutcome::Idle),
        "a11y-only 帧不得提交像素（实际 {outcome:?}）"
    );
    let record = app
        .window(window)
        .expect("window")
        .record()
        .expect("record");
    assert!(
        record.a11y_only,
        "保留的 delta 不得让 a11y-only 快路径失效（否则每次 a11y 更新都跑完整像素调度）"
    );
    assert_eq!(record.stats.a11y_only_frames, 1);
    let snapshot = app.a11y_take(window).expect("a11y 交付必须发生");
    assert_eq!(
        snapshot.node_for(label).expect("label 节点").label_text(),
        Some("新的")
    );
}

#[cfg(all(windows, feature = "test-support"))]
#[test]
fn an_idle_frame_keeps_its_scene_delta_for_the_next_scroll_frame() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let third = mount_override(
        &mut app,
        window,
        container,
        LogicalRect::new(px(0.0), px(900.0), px(200.0), px(100.0)),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(1100.0)))
        .expect("content");
    frame(&mut app, window);
    app.window_mut(window).expect("window").detach_composer();

    let patch = StylePatch {
        background: Some(Color::BLACK),
        ..StylePatch::default()
    };
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style_patch(third, patch)
    );
    assert!(
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(third)
    );
    let outcome = frame(&mut app, window);
    assert_eq!(
        outcome,
        FrameOutcome::Idle,
        "视口外节点的 damage 被裁空必须退回 Idle"
    );
    let lane = app
        .window(window)
        .expect("window")
        .tree()
        .lane_of(third)
        .expect("third row lane");
    let pending = app
        .window(window)
        .expect("window")
        .pending_delta()
        .expect("Idle 帧必须保留已构建的 SceneDelta");
    assert!(
        pending.lanes.iter().any(|delta| delta.lane == lane),
        "保留的 delta 必须包含第三行的新图元"
    );

    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(900.0)))
        .expect("scroll");
    let report = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
            None,
            None,
            None,
        )
        .expect("scroll paint");
    assert!(report.damage.rects > 0, "滚动帧必须产出视口 damage");
    let pending = app
        .window(window)
        .expect("window")
        .pending_delta()
        .expect("滚动帧必须产出待发布 delta");
    assert!(
        pending.lanes.iter().any(|delta| delta.lane == lane),
        "滚动帧的 delta 必须合并 Idle 帧保留的第三行 lane"
    );
    assert!(
        !pending.scroll.is_empty(),
        "滚动帧的 delta 必须带上滚动偏移"
    );
}

#[test]
fn scrolling_moves_the_offset_into_the_slot_table_without_touching_lanes() {
    let (_rt, mut app, window, container, child) = scrollable_app();
    let _ = app
        .window_mut(window)
        .expect("window")
        .tree_mut()
        .mark_paint(container);
    frame(&mut app, window);
    let layout_before = app
        .window(window)
        .expect("window")
        .tree()
        .get(child)
        .expect("child")
        .layout()
        .bounds;
    let before = lane_quad_bounds(&app, window, child);
    assert!(!before.is_empty(), "子节点必须有背景图元");
    let slots_before = app.window(window).expect("window").scroll_slots().to_vec();
    assert_eq!(slots_before.len(), 2, "哨兵 + 一个滚动容器");
    assert_eq!(slots_before[1].0, LogicalPoint::ZERO, "初始偏移为 0");

    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(50.0)))
        .expect("scroll");
    frame(&mut app, window);

    let after = lane_quad_bounds(&app, window, child);
    assert_eq!(
        after, before,
        "滚动不得改图元字节（偏移在槽位表里，绘制期由 GPU 平移）"
    );
    let slots_after = app.window(window).expect("window").scroll_slots().to_vec();
    assert_eq!(slots_after.len(), 2, "槽位编号逐帧稳定");
    assert_eq!(
        slots_after[1].0,
        LogicalPoint::new(px(0.0), px(50.0)),
        "滚动位移进槽位表（正数 = 内容向负方向移动）"
    );
    assert!(
        slots_after[1].1.x1 > slots_after[1].1.x0 && slots_after[1].1.y1 > slots_after[1].1.y0,
        "视口必须是非空物理矩形（滚动 damage 的来源）：{:?}",
        slots_after[1].1
    );
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .get(child)
            .expect("child")
            .layout()
            .bounds,
        layout_before,
        "layout.bounds 是内容坐标：滚动不得改它"
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset,
        LogicalPoint::new(px(0.0), px(50.0))
    );
}

#[test]
fn wheel_falls_through_to_the_outer_container_when_the_inner_cannot_scroll() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    let mut inner_style = StyleBuilder::new().size(px(200.0), px(100.0)).build();
    inner_style.clip = Clip::Bounds;
    let inner = NodeBuilder::boxed()
        .style(inner_style)
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("mount inner");
    frame(&mut app, window);

    let outer_handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("outer scrollable");
    app.set_scroll_content(&outer_handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("outer content");
    let inner_handle = app
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("inner scrollable");
    app.set_scroll_content(&inner_handle, LogicalSize::new(px(200.0), px(100.0)))
        .expect("inner content");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::scroll_lines(
            LogicalPoint::new(px(50.0), px(50.0)),
            LogicalPoint::new(px(0.0), px(-1.0)),
            Modifiers::empty(),
        ),
    );

    app.advance_clock(std::time::Duration::from_millis(400));
    frame(&mut app, window);
    let offset_of = |app: &App, node: NodeId| {
        app.window(window)
            .expect("window")
            .tree()
            .scroll_slot(node)
            .map(|slot| slot.offset)
    };
    let inner_offset = offset_of(&app, inner);
    let outer_offset = offset_of(&app, outer);
    assert_eq!(
        inner_offset.map(|offset| offset.y),
        Some(px(0.0)),
        "内层装得下内容，不得产生偏移"
    );
    assert_eq!(
        outer_offset.map(|offset| offset.y),
        Some(px(48.0)),
        "滚轮必须穿透到能滚的外层容器"
    );
}

#[test]
fn scroll_commits_a_frame() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    let before = app.window(window).expect("window").frame_index();
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(30.0)))
        .expect("scroll");
    let outcome = frame(&mut app, window);

    assert!(
        matches!(outcome, FrameOutcome::Committed(_)),
        "滚动必须提交一帧"
    );
    assert!(app.window(window).expect("window").frame_index() > before);
    assert!(!app.take_wake_request(), "帧已消费唤醒位");
}

#[test]
fn same_offset_is_a_total_noop() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(10.0)))
        .expect("scroll");
    frame(&mut app, window);
    let _ = app.take_wake_request();

    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(10.0)))
        .expect("same");

    assert!(!app.take_wake_request(), "同值滚动不得唤醒");
}

#[test]
fn hit_test_follows_the_scroll_offset() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    let before = app
        .window(window)
        .expect("window")
        .tree()
        .hit_test(LogicalPoint::new(px(50.0), px(120.0)))
        .map(|hit| hit.local_point);
    assert!(before.is_some(), "滚动前 (50,120) 命中第二个子节点");

    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(50.0)))
        .expect("scroll");
    let after = app
        .window(window)
        .expect("window")
        .tree()
        .hit_test(LogicalPoint::new(px(50.0), px(120.0)))
        .map(|hit| hit.local_point);

    assert!(after.is_some(), "滚动后同一点仍命中内容");
    assert_eq!(
        after.expect("hit").y,
        before.expect("hit").y + 50.0,
        "local_point 反应内容位移"
    );
}

#[test]
fn scroll_never_reevaluates_bind_style() {
    let (_rt, mut app, window) = setup();
    let calls = Rc::new(Cell::new(0_u32));
    let counter = calls.clone();
    let container = mount_scroll_container(&mut app, window, 100.0);
    let _child = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(200.0), px(80.0)))
        .bind_style(move |_cx| {
            counter.set(counter.get() + 1);
            StylePatch::default()
        })
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("mount");
    frame(&mut app, window);
    let evaluated_once = calls.get();
    assert!(evaluated_once > 0, "首帧必须求值一次");

    let handle = app
        .make_scroll(window, container, ScrollConfig::default())
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("content");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(20.0)))
        .expect("scroll");
    frame(&mut app, window);

    assert_eq!(calls.get(), evaluated_once, "滚动不得触发 refresh_styles");
}

fn virtual_app() -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
    let (rt, mut app, window) = setup();
    let mut style = StyleBuilder::new()
        .size(px(200.0), px(100.0))
        .relative()
        .build();
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    let container = NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount");
    frame(&mut app, window);
    (rt, app, window, container)
}

#[test]
fn virtual_rows_carry_their_position_in_the_set() {
    let (_rt, mut app, window, container) = virtual_app();
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |index| {
        NodeBuilder::boxed().id(format!("row{index}"))
    })
    .expect("items");
    frame(&mut app, window);
    let rows: Vec<(usize, NodeId)> = app
        .window(window)
        .expect("window")
        .tree()
        .virtual_slot(container)
        .expect("virtual slot")
        .mounted
        .clone();
    assert!(!rows.is_empty(), "至少挂载一行");

    let snapshot = app.a11y_take(window).expect("a11y snapshot");
    for (index, row) in rows {
        let node = snapshot
            .nodes
            .iter()
            .find(|node| node.engine == row)
            .expect("row 必须在快照里");
        assert_eq!(node.pos_in_set, Some(index), "0-based 位置");
        assert_eq!(node.size_of_set, None, "size 在容器上，不在行上");
        let ak = crate::a11y::adapter::to_accesskit(node);
        assert_eq!(ak.position_in_set(), Some(index));
    }
    let container_node = snapshot
        .nodes
        .iter()
        .find(|node| node.engine == container)
        .expect("容器必须在快照里");
    assert_eq!(container_node.size_of_set, Some(100), "集合总数在容器上");
}
#[test]
fn virtual_list_mounts_the_visible_window_and_recycles() {
    let (_rt, mut app, window, container) = virtual_app();
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    let built = Rc::new(Cell::new(0_u32));
    let counter = built.clone();
    app.set_virtual_items(&list, 100, move |index| {
        counter.set(counter.get() + 1);
        NodeBuilder::boxed()
            .paint(background())
            .id(format!("row{index}"))
    })
    .expect("items");
    frame(&mut app, window);

    let state = app.virtual_list_state(&list).expect("state");
    assert_eq!(state.count, 100);
    assert_eq!(
        (state.first, state.last),
        (0, 4),
        "视口 100 / 行高 25 ⇒ 4 行"
    );
    assert_eq!(state.mounted, 4);
    assert_eq!(built.get(), 4, "只挂载可见行");

    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(250.0)))
        .expect("scroll");
    frame(&mut app, window);
    let state = app.virtual_list_state(&list).expect("state");
    assert_eq!((state.first, state.last), (10, 14));
    assert_eq!(state.mounted, 4);
    assert_eq!(built.get(), 8, "只有新进入的 4 行被构建");
}

#[test]
fn anchor_placement_converges_with_a_root_border() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let mut root_style = Style::default();
    root_style.border(px(1.0));
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(root, root_style)
        .expect("root style");
    let anchor = mount_override(
        &mut app,
        window,
        root,
        LogicalRect::new(px(100.0), px(100.0), px(40.0), px(20.0)),
    );
    frame(&mut app, window);

    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Anchor {
                    node: anchor,
                    side: crate::interaction::AnchorSide::Below,
                    gap: 4.0,
                },
                dismiss: OverlayDismiss::Explicit,
                ..OverlayOptions::default()
            },
        )
        .expect("overlay");
    let overlay_node = overlay.node();
    let style = StyleBuilder::new()
        .absolute()
        .size(px(60.0), px(30.0))
        .build();
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(overlay_node, style)
        .expect("style");
    frame(&mut app, window);
    frame(&mut app, window);

    let bounds = app
        .window(window)
        .expect("window")
        .tree()
        .get(overlay_node)
        .expect("overlay node")
        .layout()
        .bounds;
    assert!(
        (bounds.origin.x - 100.0).abs() < px(0.5) && (bounds.origin.y - 124.0).abs() < px(0.5),
        "浮层落点必须是窗口绝对坐标（Below: y = 100+20+4）：{bounds:?}"
    );

    frame(&mut app, window);
    let _ = app.take_wake_request();
    frame(&mut app, window);
    assert!(
        !app.take_wake_request(),
        "纠位必须收敛，不得每帧都重写 inset"
    );
}

#[test]
fn anchored_above_overlay_sits_above_the_anchor() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let anchor = mount_override(
        &mut app,
        window,
        root,
        LogicalRect::new(px(100.0), px(100.0), px(40.0), px(20.0)),
    );
    frame(&mut app, window);

    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Anchor {
                    node: anchor,
                    side: crate::interaction::AnchorSide::Above,
                    gap: 4.0,
                },
                dismiss: OverlayDismiss::Explicit,
                ..OverlayOptions::default()
            },
        )
        .expect("overlay");
    let overlay_node = overlay.node();
    let style = StyleBuilder::new()
        .absolute()
        .size(px(60.0), px(30.0))
        .build();
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(overlay_node, style)
        .expect("style");
    frame(&mut app, window);
    frame(&mut app, window);
    frame(&mut app, window);

    let bounds = app
        .window(window)
        .expect("window")
        .tree()
        .get(overlay_node)
        .expect("overlay")
        .layout()
        .bounds;
    assert_eq!(
        (bounds.origin.x, bounds.origin.y, bounds.size.height),
        (px(100.0), px(66.0), px(30.0)),
        "Above：y = 锚点顶 - gap - 自身高度（下边缘贴锚点），实际 {bounds:?}"
    );
}

#[test]
fn overlay_is_last_and_closes_on_outside_click() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let _background = mount_override(
        &mut app,
        window,
        root,
        LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0)),
    );
    frame(&mut app, window);
    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(200.0), px(10.0))),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("overlay");
    let overlay_node = overlay.node();
    let style = StyleBuilder::new()
        .absolute()
        .left(px(200.0))
        .top(px(10.0))
        .size(px(80.0), px(80.0))
        .build();
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(overlay_node, style)
        .expect("style");
    frame(&mut app, window);

    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .dfs_ids()
            .last()
            .copied(),
        Some(overlay_node),
        "浮层必须是 DFS 先序的最后一个子树"
    );
    assert_eq!(app.overlays(window).len(), 1);
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .hit_test(LogicalPoint::new(px(210.0), px(20.0)))
            .map(|hit| hit.node),
        Some(overlay_node),
        "浮层优先命中"
    );

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(10.0), px(10.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert!(app.overlays(window).is_empty(), "点击外部关闭");
}

#[test]
fn dismiss_action_closes_the_topmost_overlay_including_explicit() {
    let (_rt, mut app, window) = setup();
    frame(&mut app, window);
    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::ZERO),
                dismiss: OverlayDismiss::Explicit,
                ..OverlayOptions::default()
            },
        )
        .expect("overlay");
    let root = root_of(&app, window);

    assert!(app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
    assert!(app.overlays(window).is_empty(), "Dismiss 关 Explicit 浮层");
    assert!(!app.close_overlay(&overlay), "重复关闭返回 false");
}

#[test]
fn drag_streams_deltas_and_suppresses_the_click() {
    let (_rt, mut app, window) = setup();
    let phases = Rc::new(Cell::new(Vec::<DragPhase>::new()));
    let seen = phases.clone();
    app.on_action(Action::custom("split"), move |ctx| {
        if let ActionSource::Drag { phase, .. } = ctx.source() {
            let mut list = seen.take();
            list.push(phase);
            seen.set(list);
        }
    });
    let root = root_of(&app, window);
    let grip = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(100.0), px(0.0), px(6.0), px(100.0)))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(102.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.drag_state(window), None, "未越阈值前不得成立拖拽会话");
    assert!(app.take_capture_requests().is_empty(), "按下不请求指针捕获");

    let moved = app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(122.0), px(50.0)), Modifiers::empty()),
    );
    let state = app.drag_state(window).expect("drag session");
    assert_eq!(state.node(), grip);
    assert_eq!(
        app.take_capture_requests(),
        vec![(window, true)],
        "拖拽成立时请求指针捕获"
    );
    assert_eq!(moved.action, Some(Action::custom("split")));
    assert!(
        app.drag(&state)
            .is_some_and(|info| info.delta.x == px(20.0))
    );

    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(122.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.drag_state(window), None);
    assert_eq!(app.take_capture_requests(), vec![(window, false)]);
    assert_eq!(up.action, Some(Action::custom("split")), "抬起派发 End");
    assert_eq!(up.click_count, 0, "拖拽后不产生点击");
    assert_eq!(phases.take(), vec![DragPhase::Start, DragPhase::End]);
}

#[test]
fn animation_ticks_write_the_transient_and_end_in_idle() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = mount_override(
        &mut app,
        window,
        root,
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)),
    );
    frame(&mut app, window);
    let progress = app.transient(0.0_f32);
    let id = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: progress,
                to: 1.0,
            },
            &[node],
            AnimationSpec {
                duration: std::time::Duration::from_millis(100),
                easing: Easing::Linear,
            },
        )
        .expect("animate");
    let _ = app.take_wake_request();
    app.advance_clock(std::time::Duration::from_millis(50));
    let outcome = frame(&mut app, window);

    assert!(
        (*app.read_transient(&progress) - 0.5).abs() < 1e-3,
        "中点 = 0.5"
    );
    assert!(
        matches!(outcome, FrameOutcome::Committed(_)),
        "注册节点每帧重绘 ⇒ 动画期间必然提交"
    );
    let _ = node;

    app.advance_clock(std::time::Duration::from_millis(60));
    frame(&mut app, window);
    assert_eq!(*app.read_transient(&progress), 1.0);
    assert!(!app.animations_running(window), "结束即删槽");
    assert!(!app.cancel_animation(&id), "槽已删");

    app.advance_clock(std::time::Duration::from_millis(100));
    assert!(
        matches!(frame(&mut app, window), FrameOutcome::Idle),
        "动画结束后帧回到 Idle（I3）"
    );
}

#[test]
fn closed_window_returns_unknown_window() {
    let (_rt, mut app, window) = setup();
    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::ZERO),
                dismiss: OverlayDismiss::Explicit,
                ..OverlayOptions::default()
            },
        )
        .expect("overlay");
    let transient = app.transient(0.0_f32);
    assert!(app.close_window(window));
    let _ = overlay;

    let error = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::ZERO),
                dismiss: OverlayDismiss::Explicit,
                ..OverlayOptions::default()
            },
        )
        .expect_err("窗口已关闭");
    assert!(matches!(error, EngineError::UnknownWindow { window: closed } if closed == window));

    let error = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: transient,
                to: 1.0,
            },
            &[],
            AnimationSpec::short(),
        )
        .expect_err("窗口已关闭");
    assert!(matches!(error, EngineError::UnknownWindow { .. }));
}

#[test]
fn hit_after_scrolling_50_lands_on_the_second_row() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(50.0)))
        .expect("scroll");

    let hit = app
        .window(window)
        .expect("window")
        .tree()
        .hit_test(LogicalPoint::new(px(50.0), px(60.0)))
        .expect("hit");
    assert_eq!(hit.local_point, LogicalPoint::new(px(50.0), px(10.0)));
}

#[test]
fn overlay_outside_click_consumes_the_whole_press() {
    let (_rt, mut app, window) = setup();
    let calls = Rc::new(Cell::new(0_u32));
    let seen = calls.clone();
    app.on_action(Action::custom("under"), move |_ctx| {
        seen.set(seen.get() + 1)
    });
    let root = root_of(&app, window);
    let button = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0)))
        .paint(background())
        .on_click(Action::custom("under"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.create_overlay(
        window,
        OverlayOptions {
            placement: OverlayPlacement::Point(LogicalPoint::new(px(200.0), px(10.0))),
            dismiss: OverlayDismiss::OutsideClick,
            ..OverlayOptions::default()
        },
    )
    .expect("overlay");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );

    assert!(app.overlays(window).is_empty(), "按下即关浮层");
    assert_eq!(calls.get(), 0, "这次按下必须整体消费");
    assert_eq!(up.action, None);
    assert_eq!(up.click_count, 0);
    let _ = button;
}

#[test]
fn stale_focus_is_cleared_and_tab_recovers() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let first = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("a"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let second = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(60.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("b"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(first)));

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(first)
        .expect("remove");

    assert_eq!(app.focused(window), None, "陈旧焦点必须视为无焦点");
    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Tab, Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(second));
}

#[test]
fn virtual_list_mounts_rows_on_the_first_frame_and_grows_on_resize() {
    let (_rt, mut app, window) = setup();
    let mut style = StyleBuilder::new()
        .size(px(200.0), px(100.0))
        .relative()
        .build();
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    let container = NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount");
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |index| {
        NodeBuilder::boxed()
            .paint(background())
            .id(format!("row{index}"))
    })
    .expect("items");
    frame(&mut app, window);

    let state = app.virtual_list_state(&list).expect("state");
    assert!(state.mounted > 0, "首帧后必须挂载可见行：{state:?}");
    assert_eq!((state.first, state.last), (0, 4));

    app.window_mut(window)
        .expect("window")
        .set_size(LogicalSize::new(px(400.0), px(300.0)));
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(container, {
            let mut style = StyleBuilder::new()
                .size(px(200.0), px(200.0))
                .relative()
                .build();
            style.clip = Clip::Bounds;
            style
        })
        .expect("style");
    frame(&mut app, window);
    let state = app.virtual_list_state(&list).expect("state");
    assert_eq!(
        (state.first, state.last),
        (0, 8),
        "视口 200 / 行高 25 ⇒ 8 行"
    );
    assert_eq!(state.mounted, 8);
}

#[test]
fn virtual_rows_are_laid_out_with_the_configured_extent() {
    let (_rt, mut app, window) = setup();
    let mut style = StyleBuilder::new()
        .size(px(200.0), px(100.0))
        .relative()
        .build();
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    let container = NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount");
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |index| {
        NodeBuilder::boxed()
            .paint(background())
            .id(format!("row{index}"))
    })
    .expect("items");
    frame(&mut app, window);
    frame(&mut app, window);

    let rows: Vec<(usize, crate::node::id::NodeId)> = app
        .window(window)
        .expect("window")
        .tree()
        .virtual_slot(list.node())
        .expect("slot")
        .mounted
        .clone();
    assert!(!rows.is_empty(), "必须挂载了可见行");
    for (index, row) in rows {
        let bounds = app
            .window(window)
            .expect("window")
            .tree()
            .get(row)
            .expect("row exists")
            .layout()
            .bounds;
        assert_eq!(
            bounds.size.height,
            px(25.0),
            "行 {index} 的高度必须等于 extent（实际 {bounds:?}）"
        );
        assert!(
            bounds.size.width > px(0.0),
            "行 {index} 的宽度必须 > 0（实际 {bounds:?}）"
        );
    }
}

#[test]
fn key_modifiers_reach_the_handler() {
    let (_rt, mut app, window) = setup();
    let seen = Rc::new(Cell::new(Modifiers::empty()));
    let slot = seen.clone();
    app.on_action(Action::custom("activate"), move |ctx| {
        slot.set(ctx.modifiers());
    });
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_action(Action::custom("activate"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(node)));

    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Enter, Modifiers::SHIFT | Modifiers::CONTROL),
    );

    assert_eq!(
        seen.get(),
        Modifiers::SHIFT | Modifiers::CONTROL,
        "handler 必须拿到本次按键的修饰键"
    );
}

#[test]
fn animation_retarget_replaces_the_previous_slot() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = mount_override(
        &mut app,
        window,
        root,
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)),
    );
    frame(&mut app, window);
    let value = app.transient(0.0_f32);
    let spec = AnimationSpec {
        duration: std::time::Duration::from_millis(100),
        easing: Easing::Linear,
    };
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[node],
        spec,
    )
    .expect("first");
    app.advance_clock(std::time::Duration::from_millis(50));
    frame(&mut app, window);
    assert!((*app.read_transient(&value) - 0.5).abs() < 1e-3);

    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 0.0,
        },
        &[node],
        spec,
    )
    .expect("retarget");
    app.advance_clock(std::time::Duration::from_millis(50));
    frame(&mut app, window);
    assert!(
        (*app.read_transient(&value) - 0.25).abs() < 1e-3,
        "重定向必须从当前值起步：{}",
        app.read_transient(&value)
    );

    app.advance_clock(std::time::Duration::from_millis(60));
    frame(&mut app, window);
    assert_eq!(*app.read_transient(&value), 0.0, "终点是 0，不得回弹到 1");
    assert!(!app.animations_running(window));
}

#[test]
fn zero_duration_animation_cancels_the_previous_slot_on_the_same_target() {
    let (_rt, mut app, window) = setup();
    let value = app.transient(0.0_f32);
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[],
        AnimationSpec {
            duration: std::time::Duration::from_millis(100),
            easing: Easing::Linear,
        },
    )
    .expect("slow animation");
    app.advance_clock(std::time::Duration::from_millis(50));
    frame(&mut app, window);
    assert!(
        (*app.read_transient(&value) - 0.5).abs() < 1e-3,
        "先走到中点"
    );

    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 0.25,
        },
        &[],
        AnimationSpec {
            duration: std::time::Duration::ZERO,
            easing: Easing::Linear,
        },
    )
    .expect("immediate");
    assert_eq!(*app.read_transient(&value), 0.25, "零时长必须立即写值");
    assert!(
        !app.animations_running(window),
        "零时长不得留下驱动源：旧槽必须一起取消"
    );

    app.advance_clock(std::time::Duration::from_millis(200));
    frame(&mut app, window);
    assert_eq!(
        *app.read_transient(&value),
        0.25,
        "旧动画不得在后续帧回写同一个瞬态值"
    );
}

#[test]
fn pointer_cancel_ends_the_drag_without_committing_a_click() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let phases = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = phases.clone();
    let grip = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(100.0), px(0.0), px(6.0), px(100.0)))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    app.on_action(Action::custom("split"), move |ctx| {
        if let ActionSource::Drag { phase, .. } = ctx.source() {
            seen.borrow_mut().push(phase);
        }
    });
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(102.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(160.0), px(50.0)), Modifiers::empty()),
    );
    assert!(app.drag_state(window).is_some(), "拖拽应已开始");

    app.dispatch_input(
        window,
        InputEvent::PointerCancel {
            timestamp: std::time::Instant::now(),
        },
    );

    assert!(app.drag_state(window).is_none(), "取消后不得残留拖拽会话");
    assert_eq!(
        phases.borrow().last().copied(),
        Some(DragPhase::Cancel),
        "必须派发 Cancel 阶段：{:?}",
        phases.borrow()
    );
    let _ = grip;
}

#[test]
fn second_pointer_down_during_drag_is_ignored() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let grip = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(100.0), px(0.0), px(6.0), px(100.0)))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    app.on_action(Action::custom("split"), |_ctx| {});
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(102.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(130.0), px(50.0)), Modifiers::empty()),
    );
    let before = app.drag_state(window).expect("drag");
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(300.0), px(200.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let after = app.drag_state(window).expect("drag");
    assert_eq!(before, after, "拖拽中的二次按下不得重开会话");
    assert_eq!(after.node(), grip);
}

#[test]
fn moving_past_the_drag_threshold_suppresses_the_click() {
    let (_rt, mut app, window) = setup();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = clicks.clone();
    app.on_action(Action::custom("hit"), move |_ctx| seen.set(seen.get() + 1));
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(100.0), px(100.0)))
        .paint(background())
        .on_click(Action::custom("hit"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.set_input_thresholds(InputThresholds {
        double_click_time: std::time::Duration::from_millis(500),
        double_click_distance: 4.0,
        drag_distance: 4.0,
    });

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(40.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );

    assert_eq!(clicks.get(), 0, "超过阈值 ⇒ 不是点击");
    assert_eq!(up.click_count, 0);
    let _ = node;
}

#[test]
fn stale_scroll_handle_reports_stale_node() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(container)
        .expect("remove");

    let error = app
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(10.0)))
        .expect_err("节点已卸载");
    assert!(matches!(error, EngineError::StaleNode { node } if node == container));
}

#[cfg(target_os = "windows")]
#[test]
fn ime_enabled_is_per_window_and_cleared_with_the_window() {
    let (_rt, mut app, window) = setup();
    let other = app.create_window(WindowOptions::default());
    assert!(app.ime_enabled(window), "默认启用");
    assert!(app.ime_enabled(other), "默认启用");

    app.set_ime_enabled(window, false);
    assert!(!app.ime_enabled(window), "只禁用目标窗口");
    assert!(app.ime_enabled(other), "其它窗口不受影响");

    app.set_ime_enabled(window, true);
    assert!(app.ime_enabled(window), "可以重新启用");

    app.set_ime_enabled(other, false);
    assert!(app.close_window(other));
    assert!(
        !app.ime_disabled.contains(&other),
        "关窗必须清掉 IME 旁表，否则 create/close 循环无界增长"
    );
    let third = app.create_window(WindowOptions::default());
    assert!(app.ime_enabled(third), "新窗口默认启用");
}

#[test]
fn auto_repeat_neither_fires_bindings_nor_activation() {
    let (_rt, mut app, window) = setup();
    let calls = Rc::new(Cell::new(0_u32));
    let seen = calls.clone();
    app.on_action(Action::custom("save"), move |_ctx| seen.set(seen.get() + 1));
    app.bind_key(KeyBinding {
        key: KeyCode::KeyS,
        modifiers: Modifiers::CONTROL,
        action: Action::custom("save"),
    });
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::KeyS, Modifiers::CONTROL),
    );
    assert_eq!(calls.get(), 1, "首次按下必须触发绑定");
    app.dispatch_input(
        window,
        InputEvent::Key {
            code: KeyCode::KeyS,
            state: KeyState::Down,
            repeat: true,
            modifiers: Modifiers::CONTROL,
            timestamp: std::time::Instant::now(),
        },
    );
    assert_eq!(calls.get(), 1, "自动重复不得重复触发绑定动作");

    let activations = Rc::new(Cell::new(0_u32));
    let seen = activations.clone();
    app.on_action(Action::custom("activate"), move |_ctx| {
        seen.set(seen.get() + 1)
    });
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_action(Action::custom("activate"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(node)));
    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Enter, Modifiers::empty()),
    );
    assert_eq!(activations.get(), 1);
    app.dispatch_input(
        window,
        InputEvent::Key {
            code: KeyCode::Enter,
            state: KeyState::Down,
            repeat: true,
            modifiers: Modifiers::empty(),
            timestamp: std::time::Instant::now(),
        },
    );
    assert_eq!(activations.get(), 1, "长按回车不得重复下单式激活");
}

#[test]
fn secondary_button_neither_clicks_nor_takes_focus() {
    let (_rt, mut app, window) = setup();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = clicks.clone();
    let button_source = Rc::new(Cell::new(None::<PointerButton>));
    let source_slot = button_source.clone();
    app.on_action(Action::custom("hit"), move |ctx| {
        seen.set(seen.get() + 1);
        if let ActionSource::Click { button, .. } = ctx.source() {
            source_slot.set(Some(button));
        }
    });
    let root = root_of(&app, window);
    let other = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("other"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let target = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(100.0), px(0.0), px(60.0), px(60.0)))
        .paint(background())
        .on_click(Action::custom("hit"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(other)));

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(120.0), px(20.0)),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(120.0), px(20.0)),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 0, "右键不得触发 on_click");
    assert_eq!(up.click_count, 0);
    assert_eq!(app.focused(window), Some(other), "次键不得抢焦点");
    let _ = target;

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(120.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(120.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 1);
    assert_eq!(up.click_count, 1);
    assert_eq!(button_source.get(), Some(PointerButton::Primary));
}

#[test]
fn hover_transitions_are_observable_and_mark_paint() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let outer_style = StyleBuilder::new().size(px(100.0), px(100.0)).build();
    let outer = NodeBuilder::boxed()
        .style(outer_style)
        .bind_style(|_| StylePatch::default())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let inner = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(40.0), px(40.0)))
        .bind_style(|_| StylePatch::default())
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("mount");
    frame(&mut app, window);
    let _ = app.drain_events();
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .clear_all_dirty_for_test();

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(20.0), px(20.0)), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), Some(inner), "悬停读点必须可用");
    let (hovered, entered, left) = app
        .drain_events()
        .into_iter()
        .find_map(|(_, event)| match event {
            Event::HoverChanged {
                hovered,
                entered,
                left,
            } => Some((hovered, entered, left)),
            _ => None,
        })
        .expect("悬停变化必须派生事件");
    assert_eq!(hovered, Some(inner));
    assert_eq!(entered.last().copied(), Some(inner));
    assert!(
        entered.contains(&outer) && entered.contains(&root),
        "祖先与根也必须收到 enter：{entered:?}"
    );
    assert!(left.is_empty());
    {
        let tree = app.window(window).expect("window").tree();
        for node in [outer, inner] {
            assert!(
                tree.get(node)
                    .expect("node")
                    .dirty()
                    .contains(DirtyFlags::PAINT),
                "悬停变化节点必须标 PAINT，否则 bind_style 不会重算"
            );
        }
    }

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(500.0), px(500.0)), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), None);
    let (_, entered, left) = app
        .drain_events()
        .into_iter()
        .find_map(|(_, event)| match event {
            Event::HoverChanged {
                hovered,
                entered,
                left,
            } => Some((hovered, entered, left)),
            _ => None,
        })
        .expect("离开也要派生事件");
    assert!(entered.is_empty());
    assert_eq!(left.first().copied(), Some(inner));
    assert_eq!(left.last().copied(), Some(root));
    assert!(left.contains(&outer));
}

#[test]
fn a_press_below_the_drag_threshold_is_a_click() {
    let (_rt, mut app, window) = setup();
    let phases = Rc::new(Cell::new(Vec::<DragPhase>::new()));
    let seen = phases.clone();
    app.on_action(Action::custom("split"), move |ctx| {
        if let ActionSource::Drag { phase, .. } = ctx.source() {
            let mut list = seen.take();
            list.push(phase);
            seen.set(list);
        }
    });
    let hits = Rc::new(Cell::new(0_u32));
    let seen = hits.clone();
    app.on_action(Action::custom("click"), move |_ctx| {
        seen.set(seen.get() + 1)
    });
    let root = root_of(&app, window);
    let grip = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(100.0), px(0.0), px(60.0), px(100.0)))
        .on_click(Action::custom("click"))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.set_input_thresholds(InputThresholds {
        double_click_time: std::time::Duration::from_millis(500),
        double_click_distance: 4.0,
        drag_distance: 4.0,
    });

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(120.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(121.0), px(51.0)), Modifiers::empty()),
    );
    assert!(phases.take().is_empty(), "未越阈值不得派发 Drag");
    assert_eq!(app.drag_state(window), None);
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(121.0), px(51.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(hits.get(), 1, "未开始拖拽 ⇒ 仍然是点击");
    let _ = grip;
}

#[test]
fn the_drag_start_waits_for_the_threshold_and_carries_the_real_delta() {
    let (_rt, mut app, window) = setup();
    let starts = Rc::new(Cell::new(Vec::<f32>::new()));
    let seen = starts.clone();
    app.on_action(Action::custom("split"), move |ctx| {
        if let ActionSource::Drag { phase, delta } = ctx.source()
            && phase == DragPhase::Start
        {
            let mut list = seen.take();
            list.push(delta.x.value());
            seen.set(list);
        }
    });
    let root = root_of(&app, window);
    let grip = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(100.0), px(0.0), px(60.0), px(100.0)))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    let _ = app.take_capture_requests();

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(120.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert!(
        app.take_capture_requests().is_empty(),
        "按下不请求捕获（还没确认拖拽意图）"
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(124.0), px(50.0)), Modifiers::empty()),
    );
    assert!(starts.take().is_empty(), "4 px 恰好不越阈值");
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(140.0), px(50.0)), Modifiers::empty()),
    );
    assert_eq!(starts.take(), vec![20.0], "Start 必须带真实位移");
    assert_eq!(app.take_capture_requests(), vec![(window, true)]);
    let _ = grip;
}

#[test]
fn pointer_capture_api_has_an_explicit_lifecycle() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);

    let stale = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .remove(stale)
        .expect("remove");
    assert!(
        !app.capture_pointer(window, stale),
        "已卸载的节点不得成为捕获目标"
    );
    assert!(app.capture_pointer(window, node));
    assert_eq!(app.pointer_captured(window), Some(node));
    assert_eq!(app.take_capture_requests(), vec![(window, true)]);
    assert!(
        !app.capture_pointer(window, node),
        "同一节点重复捕获是幂等的"
    );
    assert!(app.release_pointer(window));
    assert_eq!(app.pointer_captured(window), None);
    assert_eq!(app.take_capture_requests(), vec![(window, false)]);
    assert!(!app.release_pointer(window), "没有捕获时释放返回 false");
}

#[test]
fn key_up_lands_and_arrow_keys_scroll_the_focused_container() {
    let (_rt, mut app, window) = setup();
    let container = scrollable_container(&mut app, window);
    let button = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("row"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("mount");
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("content");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(button)));

    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty()),
    );
    assert!(
        app.key_down(window, KeyCode::ArrowDown),
        "按下期间必须可查询"
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        px(48.0),
        "方向键默认滚动一行"
    );
    app.dispatch_input(
        window,
        InputEvent::key_up(KeyCode::ArrowDown, Modifiers::empty()),
    );
    assert!(
        !app.key_down(window, KeyCode::ArrowDown),
        "抬起后不再是按住"
    );

    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::PageDown, Modifiers::empty()),
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        px(148.0)
    );

    let intercepted = Rc::new(Cell::new(0_u32));
    let seen = intercepted.clone();
    app.on_action(Action::custom("intercept"), move |_ctx| {
        seen.set(seen.get() + 1)
    });
    app.bind_key(KeyBinding {
        key: KeyCode::Home,
        modifiers: Modifiers::empty(),
        action: Action::custom("intercept"),
    });
    let before = app.scroll_state(&handle).expect("state").offset.y;
    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Home, Modifiers::empty()),
    );
    assert_eq!(intercepted.get(), 1, "绑定优先于默认滚动");
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        before,
        "被拦截后不得再走默认滚动"
    );
}

#[test]
fn a_modal_overlay_traps_focus_and_restores_it_on_close() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let opener = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("opener"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let background = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(50.0), px(0.0), px(40.0), px(40.0)))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(opener)));
    let _ = app.drain_events();

    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(100.0), px(100.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("overlay");
    let first = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(110.0), px(110.0), px(40.0), px(20.0)))
        .on_click(Action::custom("first"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            overlay.node(),
        )
        .expect("mount");
    let second = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(160.0), px(110.0), px(40.0), px(20.0)))
        .on_click(Action::custom("second"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            overlay.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    assert!(
        !app.set_focus(window, Some(background)),
        "模态外节点不可聚焦"
    );
    assert_eq!(
        app.focused(window),
        None,
        "模态打开后作用域外的焦点不得再被报告"
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(5.0), px(5.0)), Modifiers::empty()),
    );
    assert_eq!(
        app.focused(window),
        Some(first),
        "焦点修复：移入模态内第一个可聚焦节点"
    );
    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Tab, Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(second));
    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Tab, Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(first), "Tab 在浮层内回绕");

    assert!(app.close_overlay(&overlay));
    assert_eq!(
        app.focused(window),
        Some(opener),
        "关闭必须恢复 opener 焦点"
    );
}

#[test]
fn the_event_buffer_is_bounded() {
    let (_rt, mut app, window) = setup();
    let capacity = super::event_queue::EVENT_QUEUE_CAPACITY;
    for _ in 0..capacity + 3 {
        app.dispatch(window, Event::CloseRequested);
    }
    assert_eq!(app.drain_events().len(), capacity, "容量是硬上限");
    assert_eq!(app.events_dropped(), 3, "溢出条数必须可观测");
}

#[test]
fn pixel_scroll_follows_immediately_while_line_scroll_is_animated() {
    let (_rt, mut app, window) = setup();
    let container = scrollable_container(&mut app, window);
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("content");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::scroll_lines(
            LogicalPoint::new(px(50.0), px(50.0)),
            LogicalPoint::new(px(0.0), px(-1.0)),
            Modifiers::empty(),
        ),
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        px(0.0),
        "行单位走动画（同帧内还没动）"
    );

    app.dispatch_input(
        window,
        InputEvent::Scroll {
            position: LogicalPoint::new(px(50.0), px(50.0)),
            delta: LogicalPoint::new(px(0.0), px(-30.0)),
            unit: ScrollUnit::Pixel,
            phase: ScrollPhase::Update,
            modifiers: Modifiers::empty(),
            timestamp: std::time::Instant::now(),
        },
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        px(30.0),
        "像素单位必须立即到位"
    );
}

#[test]
fn click_count_uses_the_event_time_stamp() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0)))
        .paint(background())
        .on_click(Action::custom("hit"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.set_input_thresholds(InputThresholds {
        double_click_time: std::time::Duration::from_millis(500),
        double_click_distance: 4.0,
        drag_distance: 4.0,
    });
    let base = std::time::Instant::now();
    let click = |app: &mut App, at: std::time::Instant| {
        app.dispatch_input(
            window,
            InputEvent::pointer_down(
                LogicalPoint::new(px(20.0), px(20.0)),
                PointerButton::Primary,
                Modifiers::empty(),
            )
            .with_timestamp(at),
        );
        app.dispatch_input(
            window,
            InputEvent::pointer_up(
                LogicalPoint::new(px(20.0), px(20.0)),
                PointerButton::Primary,
                Modifiers::empty(),
            )
            .with_timestamp(at),
        )
    };
    assert_eq!(click(&mut app, base).click_count, 1);
    assert_eq!(
        click(&mut app, base + std::time::Duration::from_millis(100)).click_count,
        2
    );
    assert_eq!(
        click(&mut app, base + std::time::Duration::from_millis(1_100)).click_count,
        1,
        "阈值必须按事件时刻判定，不是分发时刻"
    );
    let _ = node;
}

fn scrollable_container(app: &mut App, window: WindowId) -> NodeId {
    let mut style = StyleBuilder::new().size(px(100.0), px(100.0)).build();
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount")
}

#[test]
fn a_popup_opened_from_a_modal_receives_clicks() {
    let (_rt, mut app, window) = setup();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = clicks.clone();
    app.on_action(Action::custom("menu.item"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let root = root_of(&app, window);
    let background = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(400.0), px(300.0)))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(background)));

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(20.0), px(20.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let confirm = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(80.0), px(24.0)))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(5.0), px(5.0)), Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(confirm), "焦点必须移入模态");

    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(120.0), px(60.0))),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let item = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(100.0), px(20.0)))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    let position = LogicalPoint::new(px(140.0), px(78.0));
    assert_eq!(
        app.hit_at(window, position).map(|hit| hit.node),
        Some(item),
        "命中必须落在菜单项上"
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_down(position, PointerButton::Primary, Modifiers::empty()),
    );
    let outcome = app.dispatch_input(
        window,
        InputEvent::pointer_up(position, PointerButton::Primary, Modifiers::empty()),
    );
    assert_eq!(outcome.click_count, 1, "模态之上的菜单项必须收到点击");
    assert_eq!(clicks.get(), 1, "菜单项的动作必须派发");
}

#[test]
fn a_popup_opened_in_the_same_callback_as_its_modal_receives_clicks() {
    let (_rt, mut app, window) = setup();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = clicks.clone();
    app.on_action(Action::custom("menu.item"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let root = root_of(&app, window);
    let background_button = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(200.0), px(100.0)))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(background_button)));

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(20.0), px(20.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(120.0), px(60.0))),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let confirm = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(30.0), px(30.0), px(60.0), px(24.0)))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    let item = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(100.0), px(20.0)))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    assert_eq!(
        app.focused(window),
        None,
        "模态打开后作用域外的焦点视为无焦点"
    );

    let position = LogicalPoint::new(px(140.0), px(78.0));
    assert_eq!(
        app.hit_at(window, position).map(|hit| hit.node),
        Some(item),
        "命中必须落在菜单项上"
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_down(position, PointerButton::Primary, Modifiers::empty()),
    );
    let outcome = app.dispatch_input(
        window,
        InputEvent::pointer_up(position, PointerButton::Primary, Modifiers::empty()),
    );
    assert_eq!(outcome.click_count, 1, "模态之后立即打开的菜单必须可点");
    assert_eq!(outcome.target, Some(item));
    assert_eq!(clicks.get(), 1, "菜单项的动作必须派发");
    let _ = confirm;
}

#[test]
fn a_popup_opened_without_any_focus_still_receives_clicks() {
    let (_rt, mut app, window) = setup();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = clicks.clone();
    app.on_action(Action::custom("menu.item"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let root = root_of(&app, window);
    let _background = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(200.0), px(100.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert_eq!(app.focused(window), None, "前置：没有焦点");

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(20.0), px(20.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(120.0), px(60.0))),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let _confirm = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(30.0), px(30.0), px(60.0), px(24.0)))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    let item = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(100.0), px(20.0)))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    let position = LogicalPoint::new(px(140.0), px(78.0));
    assert_eq!(app.hit_at(window, position).map(|hit| hit.node), Some(item));
    app.dispatch_input(
        window,
        InputEvent::pointer_down(position, PointerButton::Primary, Modifiers::empty()),
    );
    let outcome = app.dispatch_input(
        window,
        InputEvent::pointer_up(position, PointerButton::Primary, Modifiers::empty()),
    );
    assert_eq!(outcome.click_count, 1, "无焦点时打开的菜单同样必须可点");
    assert_eq!(clicks.get(), 1);
}

#[test]
fn a_modal_opened_without_any_focus_fills_the_first_focusable_node() {
    let (_rt, mut app, window) = setup();
    let activations = Rc::new(Cell::new(0_u32));
    let seen = activations.clone();
    app.on_action(Action::custom("modal.ok"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let root = root_of(&app, window);
    let chart = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(200.0), px(100.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(10.0), px(10.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(10.0), px(10.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.focused(window), None, "前置：图表区不抢焦点");
    let _ = chart;

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(40.0), px(40.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let ok = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(60.0), px(24.0)))
        .on_click(Action::custom("modal.ok"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);
    assert_eq!(
        app.focused(window),
        None,
        "创建瞬间还没有候选（子节点在 create_overlay 之后才挂载）"
    );

    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Enter, Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(ok), "Enter 前焦点必须已移入模态");
    assert_eq!(outcome.target, Some(ok), "Enter 必须有落点");
    assert_eq!(activations.get(), 1);
}

#[test]
fn enter_cannot_activate_a_background_button_behind_a_modal() {
    let (_rt, mut app, window) = setup();
    let background_hits = Rc::new(Cell::new(0_u32));
    let modal_hits = Rc::new(Cell::new(0_u32));
    let seen_background = background_hits.clone();
    app.on_action(Action::custom("background"), move |_ctx| {
        seen_background.set(seen_background.get() + 1);
    });
    let seen_modal = modal_hits.clone();
    app.on_action(Action::custom("modal.ok"), move |_ctx| {
        seen_modal.set(seen_modal.get() + 1);
    });

    let root = root_of(&app, window);
    let background = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(40.0)))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(background)));

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(200.0), px(100.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let ok = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(60.0), px(24.0)))
        .on_click(Action::custom("modal.ok"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Enter, Modifiers::empty()),
    );
    assert_eq!(background_hits.get(), 0, "背景按钮不得被 Enter 激活");
    assert_eq!(
        outcome.target,
        Some(ok),
        "Enter 必须激活模态内第一个可聚焦节点"
    );
    assert_eq!(modal_hits.get(), 1);

    assert!(
        !app.dispatch_action(
            window,
            background,
            Action::custom("background"),
            ActionSource::Programmatic,
        ),
        "作用域外的程序化动作必须被拒绝"
    );
    assert_eq!(background_hits.get(), 0);
    assert!(app.dispatch_action(
        window,
        ok,
        Action::custom("modal.ok"),
        ActionSource::Programmatic
    ));
    assert_eq!(modal_hits.get(), 2);

    let root = root_of(&app, window);
    assert!(app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
    assert!(app.overlays(window).is_empty(), "Dismiss 必须关掉模态");
}

#[test]
fn removing_the_captured_node_releases_the_capture() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let panel = NodeBuilder::boxed()
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let handle = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(20.0), px(20.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), panel)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.capture_pointer(window, handle));
    assert_eq!(app.pointer_captured(window), Some(handle));
    let _ = app.take_capture_requests();
    let _ = app.drain_events();

    assert!(app.remove_node(window, panel).is_ok());
    assert_eq!(
        app.pointer_captured(window),
        None,
        "不得返回已卸载的捕获目标"
    );
    assert_eq!(
        app.take_capture_requests(),
        vec![(window, false)],
        "必须请求平台释放捕获"
    );
    let events = app.drain_events();
    assert!(
        events
            .iter()
            .any(|(_, event)| matches!(event, Event::Input(InputEvent::PointerCancel { .. }))),
        "必须派发 PointerCancel 观测事件：{events:?}"
    );
}

#[test]
fn programmatic_scroll_cancels_a_pending_wheel_animation() {
    let (_rt, mut app, window) = setup();
    let container = scrollable_container(&mut app, window);
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(100.0), px(400.0)))
        .expect("content");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::scroll_lines(
            LogicalPoint::new(px(50.0), px(50.0)),
            LogicalPoint::new(px(0.0), px(-1.0)),
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.scroll_state(&handle).expect("state").offset.y, px(0.0));
    assert!(app.animations_running(window), "滚轮动画已登记");

    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(300.0)))
        .expect("scroll");
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        px(300.0)
    );
    assert!(
        !app.animations_running(window),
        "scroll_to 必须取消同槽动画"
    );

    app.advance_clock(std::time::Duration::from_millis(200));
    frame(&mut app, window);
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        px(300.0),
        "残留动画不得回弹"
    );
}

#[test]
fn held_keys_are_per_window_and_cleared_on_close() {
    let (_rt, mut app, window) = setup();
    let other = app.create_window(WindowOptions {
        title: "other".into(),
        size: LogicalSize::new(px(200.0), px(200.0)),
        ..WindowOptions::default()
    });
    frame(&mut app, window);
    frame(&mut app, other);

    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::KeyA, Modifiers::empty()),
    );
    assert!(app.key_down(window, KeyCode::KeyA));
    assert!(
        !app.key_down(other, KeyCode::KeyA),
        "A 窗按住的键不得让 B 窗的查询为真"
    );

    assert!(app.close_window(window));
    assert!(
        !app.key_down(window, KeyCode::KeyA),
        "关窗必须清理该窗口的按住集合"
    );
}

#[test]
fn only_the_primary_button_drives_drag_sessions() {
    let (_rt, mut app, window) = setup();
    let phases = Rc::new(Cell::new(Vec::<DragPhase>::new()));
    let seen = phases.clone();
    app.on_action(Action::custom("grip"), move |ctx| {
        if let ActionSource::Drag { phase, .. } = ctx.source() {
            let mut list = seen.take();
            list.push(phase);
            seen.set(list);
        }
    });
    let root = root_of(&app, window);
    let grip = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0)))
        .on_drag(Action::custom("grip"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(10.0), px(10.0)),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(40.0), px(10.0)), Modifiers::empty()),
    );
    assert_eq!(app.drag_state(window), None, "次键不得启动拖拽");
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(40.0), px(10.0)),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(10.0), px(10.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(40.0), px(10.0)), Modifiers::empty()),
    );
    assert_eq!(
        app.drag_state(window).map(|state| state.node()),
        Some(grip),
        "主键必须启动拖拽"
    );

    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(40.0), px(10.0)),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    assert!(
        app.drag_state(window).is_some(),
        "次键抬起不得结束主键的拖拽"
    );
    assert_eq!(phases.take(), vec![DragPhase::Start]);

    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(40.0), px(10.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.drag_state(window), None);
    assert_eq!(phases.take(), vec![DragPhase::End]);
}

#[test]
fn a_two_dimensional_wheel_delta_is_split_across_the_scroll_chain() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 100.0);
    let mut inner_style = StyleBuilder::new().size(px(100.0), px(50.0)).build();
    inner_style.clip = Clip::Bounds;
    let inner = NodeBuilder::boxed()
        .style(inner_style)
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("mount");
    frame(&mut app, window);

    let outer_handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("outer scrollable");
    app.set_scroll_content(&outer_handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("outer content");
    let inner_handle = app
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Horizontal,
                line_height: px(48.0),
            },
        )
        .expect("inner scrollable");
    app.set_scroll_content(&inner_handle, LogicalSize::new(px(400.0), px(50.0)))
        .expect("inner content");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::Scroll {
            position: LogicalPoint::new(px(20.0), px(20.0)),
            delta: LogicalPoint::new(px(-12.0), px(-3.0)),
            unit: ScrollUnit::Pixel,
            phase: ScrollPhase::Update,
            modifiers: Modifiers::empty(),
            timestamp: std::time::Instant::now(),
        },
    );
    assert_eq!(
        app.scroll_state(&inner_handle)
            .expect("inner state")
            .offset
            .x,
        px(12.0),
        "内层横向容器消费 dx"
    );
    assert_eq!(
        app.scroll_state(&outer_handle)
            .expect("outer state")
            .offset
            .y,
        px(3.0),
        "纵向分量必须传给外层容器"
    );
}

#[test]
fn an_overlay_a11y_role_is_declared_explicitly_and_the_modal_scope_is_queryable() {
    let (_rt, mut app, window) = setup();
    frame(&mut app, window);
    assert_eq!(app.modal_scope(window), None, "没有模态浮层");

    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(120.0), px(60.0))),
                dismiss: OverlayDismiss::OutsideClick,
                a11y: crate::a11y::A11yOverride {
                    role: Some(crate::a11y::A11yRole::Menu),
                    label: Some(smol_str::SmolStr::new_static("交易菜单")),
                    ..crate::a11y::A11yOverride::default()
                },
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    frame(&mut app, window);
    let snapshot = app.a11y_take(window).expect("首笔 a11y 交付");
    let menu_record = snapshot
        .node_for(menu.node())
        .expect("浮层包装必须出现在 a11y 树里");
    assert_eq!(
        menu_record.role,
        crate::a11y::A11yRole::Menu,
        "显式 Menu 角色必须生效（不得被浮层 kind 覆盖成 Dialog）"
    );
    assert_eq!(menu_record.label_text(), Some("交易菜单"));
    assert!(
        menu_record.actions.is_empty(),
        "菜单包装不得自动宣告 Expand/Collapse：{:?}",
        menu_record.actions
    );
    assert!(!menu_record.hidden);

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(20.0), px(20.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                a11y: crate::a11y::A11yOverride {
                    role: Some(crate::a11y::A11yRole::Dialog),
                    label: Some(smol_str::SmolStr::new_static("确认下单")),
                    ..crate::a11y::A11yOverride::default()
                },
                ..OverlayOptions::default()
            },
        )
        .expect("modal");
    assert_eq!(
        app.modal_scope(window),
        Some(modal.node()),
        "模态作用域根必须可查询"
    );
    let confirm = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(80.0), px(24.0)))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);
    let snapshot = app.a11y_take(window).expect("模态打开必须交付 a11y 增量");
    let modal_record = snapshot.node_for(modal.node()).expect("模态包装");
    assert_eq!(modal_record.role, crate::a11y::A11yRole::Dialog);
    assert!(
        modal_record.modal,
        "a11y modal 必须跟 trap_focus 走（引擎真值），与角色无关"
    );
    assert!(!modal_record.hidden);
    assert!(
        !snapshot.node_for(confirm).expect("确认按钮").hidden,
        "模态子树对 AT 必须可见"
    );
    assert!(
        snapshot.node_for(menu.node()).expect("菜单").hidden,
        "模态作用域外的浮层必须 aria-hidden（tooltip / 菜单不得被 AT 读到）"
    );

    assert!(app.close_overlay(&modal));
    assert_eq!(app.modal_scope(window), None);
    frame(&mut app, window);
    let snapshot = app.a11y_take(window).expect("模态关闭必须交付 a11y 增量");
    assert!(
        !snapshot.node_for(menu.node()).expect("菜单").hidden,
        "模态关闭后背景必须重新可见"
    );
}

#[test]
fn a_modal_filters_background_hover_so_tooltips_cannot_open_over_it() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let trigger = mount_override(
        &mut app,
        window,
        root,
        LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0)),
    );
    frame(&mut app, window);
    let _ = app.drain_events();

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(50.0), px(50.0)), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), Some(trigger));
    let _ = app.drain_events();

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(200.0), px(0.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                ..OverlayOptions::default()
            },
        )
        .expect("modal");
    let confirm = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(80.0), px(40.0)))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    assert_eq!(
        app.hovered(window),
        None,
        "模态一打开，背景悬停必须立即失效（tooltip 不得留在模态之上）"
    );
    let hover_event = app
        .drain_events()
        .into_iter()
        .find_map(|(_, event)| match event {
            Event::HoverChanged { hovered, left, .. } => Some((hovered, left)),
            _ => None,
        })
        .expect("作用域外悬停结束必须派生 HoverChanged");
    assert_eq!(hover_event.0, None);
    assert!(
        hover_event.1.contains(&trigger),
        "背景触发器必须收到 leave：{:?}",
        hover_event.1
    );

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(60.0), px(60.0)), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), None);
    assert!(
        app.drain_events()
            .into_iter()
            .all(|(_, event)| !matches!(event, Event::HoverChanged { .. })),
        "背景被模态挡住时不得产生悬停迁移事件"
    );

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(215.0), px(15.0)), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), Some(confirm), "模态内部的悬停必须正常");

    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(300.0), px(100.0))),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let item = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(100.0), px(20.0)))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(320.0), px(118.0)), Modifiers::empty()),
    );
    assert_eq!(
        app.hovered(window),
        Some(item),
        "属于模态的弹出层必须可悬停（对话框里的下拉菜单）"
    );
}

#[test]
fn a_modal_blocks_background_clicks_and_restores_interaction_on_close() {
    let (_rt, mut app, window) = setup();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = clicks.clone();
    app.on_action(Action::custom("background"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let ok_clicks = Rc::new(Cell::new(0_u32));
    let seen_ok = ok_clicks.clone();
    app.on_action(Action::custom("modal.ok"), move |_ctx| {
        seen_ok.set(seen_ok.get() + 1);
    });
    let root = root_of(&app, window);
    let background = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0)))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(background)));

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(LogicalPoint::new(px(150.0), px(50.0))),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                ..OverlayOptions::default()
            },
        )
        .expect("modal");
    let ok = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(60.0), px(24.0)))
        .on_click(Action::custom("modal.ok"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    let snapshot = app.a11y_take(window).expect("a11y 交付");
    assert_ne!(
        snapshot.focus_node().engine,
        background,
        "作用域外的背景不得成为 a11y 焦点"
    );

    assert!(
        !app.set_focus(window, Some(background)),
        "模态作用域外的节点不得获得焦点"
    );
    app.focus_next(window);
    assert_eq!(app.focused(window), Some(ok), "Tab 必须落在模态内");

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(50.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(50.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 0, "模态作用域外的点击不得派发");

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(170.0), px(70.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(170.0), px(70.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(ok_clicks.get(), 1, "模态内的点击必须正常派发");

    assert!(app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
    assert_eq!(app.modal_scope(window), None, "Esc/Dismiss 必须关掉模态");

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(50.0), px(50.0)), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), Some(background));
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(50.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(50.0), px(50.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 1, "模态关闭后背景点击必须恢复");
}

fn mount_wheel_pane(app: &mut App, window: WindowId, outer: NodeId, height: f32) -> NodeId {
    let mut style = Style::default();
    style.clip = Clip::Bounds;
    NodeBuilder::boxed()
        .style(style)
        .layout_override(LogicalRect::new(px(0.0), px(0.0), px(200.0), px(height)))
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("mount inner pane")
}

fn nested_wheel_app(
    inner_content: f32,
    outer_content: f32,
) -> (tokio::runtime::Runtime, App, WindowId, NodeId, NodeId) {
    let (rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    let inner = mount_wheel_pane(&mut app, window, outer, 100.0);
    frame(&mut app, window);
    let outer_handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("outer scrollable");
    app.set_scroll_content(
        &outer_handle,
        LogicalSize::new(px(200.0), px(outer_content)),
    )
    .expect("outer content");
    let inner_handle = app
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("inner scrollable");
    app.set_scroll_content(
        &inner_handle,
        LogicalSize::new(px(200.0), px(inner_content)),
    )
    .expect("inner content");
    frame(&mut app, window);
    (rt, app, window, outer, inner)
}

fn dispatch_wheel(
    app: &mut App,
    window: WindowId,
    position: LogicalPoint,
    lines: f32,
    timestamp: std::time::Instant,
) {
    app.dispatch_input(
        window,
        InputEvent::Scroll {
            position,
            delta: LogicalPoint::new(px(0.0), px(lines)),
            unit: ScrollUnit::Line,
            phase: ScrollPhase::Discrete,
            modifiers: Modifiers::empty(),
            timestamp,
        },
    );
}

fn settle_frames(app: &mut App, window: WindowId) {
    app.advance_clock(std::time::Duration::from_millis(400));
    frame(app, window);
}

fn wheel_offset(app: &App, window: WindowId, node: NodeId) -> Option<LogicalPoint> {
    app.window(window)
        .expect("window")
        .tree()
        .scroll_slot(node)
        .map(|slot| slot.offset)
}

#[test]
fn wheel_over_inner_content_scrolls_only_inner() {
    let (_rt, mut app, window, outer, inner) = nested_wheel_app(400.0, 400.0);
    let base = std::time::Instant::now();
    for step in 0..3 {
        dispatch_wheel(
            &mut app,
            window,
            LogicalPoint::new(px(50.0), px(50.0)),
            -1.0,
            base + std::time::Duration::from_millis(50 * step),
        );
    }
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(144.0))),
        "内层内容上的 3 行滚轮只滚内层"
    );
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::ZERO),
        "同一序列外层必须保持不动"
    );
}

#[test]
#[ignore = "P0 red until P2"]
fn line_wheel_applies_immediately_and_accumulates_in_one_frame() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("content");
    frame(&mut app, window);

    let base = std::time::Instant::now();
    for step in 0..2 {
        dispatch_wheel(
            &mut app,
            window,
            LogicalPoint::new(px(50.0), px(50.0)),
            -1.0,
            base + std::time::Duration::from_millis(50 * step),
        );
    }
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::new(px(0.0), px(96.0))),
        "行滚轮必须立即到位，同一帧内两次刻度累加为 96 px，不得有待等待动画"
    );
    assert!(
        !app.has_animations(),
        "行滚轮必须立即到位，不得创建 AnimationTarget::Scroll"
    );
}

#[test]
fn pixel_wheel_applies_immediately() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(48.0),
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("content");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::Scroll {
            position: LogicalPoint::new(px(50.0), px(50.0)),
            delta: LogicalPoint::new(px(0.0), px(-30.0)),
            unit: ScrollUnit::Pixel,
            phase: ScrollPhase::Discrete,
            modifiers: Modifiers::empty(),
            timestamp: std::time::Instant::now(),
        },
    );
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::new(px(0.0), px(30.0))),
        "单轴 Pixel 滚轮必须同帧立即到位"
    );
    assert!(
        !app.has_animations(),
        "Pixel 滚轮不得创建 AnimationTarget::Scroll"
    );
}

#[test]
#[ignore = "P0 red until P2"]
fn inner_at_end_does_not_move_outer_in_same_sequence() {
    let (_rt, mut app, window, outer, inner) = nested_wheel_app(148.0, 400.0);
    let base = std::time::Instant::now();
    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(50.0)),
        -1.0,
        base,
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "前置条件：内层已到边界"
    );

    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(50.0)),
        -1.0,
        base + std::time::Duration::from_millis(50),
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "内层停在边界"
    );
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::ZERO),
        "同一序列的余量必须丢弃：外层不得移动"
    );
}

#[test]
#[ignore = "P0 red until P2"]
fn new_sequence_after_idle_moves_outer_when_inner_at_end() {
    let (_rt, mut app, window, outer, inner) = nested_wheel_app(148.0, 400.0);
    let base = std::time::Instant::now();
    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(50.0)),
        -1.0,
        base,
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "前置条件：内层已到边界"
    );

    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(50.0)),
        -1.0,
        base + std::time::Duration::from_millis(50),
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "同一序列内层停在边界"
    );
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::ZERO),
        "同一序列的余量必须丢弃：旧链式会结转并让外层离开 0"
    );

    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(50.0)),
        -1.0,
        base + std::time::Duration::from_millis(500),
    );
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "新序列不从内层边界继续"
    );
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "间隔超过序列空闲阈值后重新仲裁：外层整包消费 48 px 并立即到位（旧动画实现此处仍是 0）"
    );
}

#[test]
#[ignore = "P0 red until P2"]
fn wheel_sequence_latches_target_when_pointer_moves() {
    let (_rt, mut app, window, outer, inner) = nested_wheel_app(400.0, 400.0);
    let base = std::time::Instant::now();
    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(50.0)),
        -1.0,
        base,
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "前置条件：内层已滚 1 行"
    );

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(50.0), px(120.0)), Modifiers::empty()),
    );
    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(120.0)),
        -1.0,
        base + std::time::Duration::from_millis(50),
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(96.0))),
        "指针移到页面后同一序列仍锁定内层"
    );
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::ZERO),
        "序列内不得因指针移动改滚外层"
    );
}

#[test]
fn new_sequence_after_idle_retargets_to_hovered_container() {
    let (_rt, mut app, window, outer, inner) = nested_wheel_app(400.0, 400.0);
    let base = std::time::Instant::now();
    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(50.0)),
        -1.0,
        base,
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "前置条件：内层已滚 1 行"
    );

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(50.0), px(120.0)), Modifiers::empty()),
    );
    dispatch_wheel(
        &mut app,
        window,
        LogicalPoint::new(px(50.0), px(120.0)),
        -1.0,
        base + std::time::Duration::from_millis(500),
    );
    settle_frames(&mut app, window);
    assert_eq!(
        wheel_offset(&app, window, inner),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "新序列必须重新命中：内层不再接收"
    );
    assert_eq!(
        wheel_offset(&app, window, outer),
        Some(LogicalPoint::new(px(0.0), px(48.0))),
        "新序列按指针位置重定位到外层"
    );
}

use std::{cell::Cell, rc::Rc};

use super::App;
use crate::{
    error::EngineError,
    event::{
        Action, ActionSource, InputEvent, Key, KeyBinding, KeyState, Modifiers, PointerButton,
    },
    interaction::{
        AnimatedValue, AnimationSpec, DragPhase, Easing, InputThresholds, OverlayDismiss,
        OverlayOptions, OverlayPlacement, ScrollAxis, ScrollConfig, VirtualListConfig,
    },
    math::{Point, Rect, Scale, Size},
    node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
    paint::{
        color::Color,
        primitive::Primitive,
        style::{Clip, FocusRing, PaintSpec, Style, StylePatch},
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
        size: Size::new(400.0, 300.0),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    (rt, app, window)
}

fn frame(app: &mut App, window: WindowId) -> FrameOutcome {
    app.frame(window)
        .expect("window exists")
        .expect("frame succeeds")
}

/// P2-3.2 回归：深层节点的布局变化**不得**把祖先（尤其根）的整块盒子并入 damage。
///
/// 旧实现的 `bubble_layout` 复用 `touch(LAYOUT)`，每个祖先都会把自己的盒子并进
/// damage，一路到根 ⇒ 根盒 = 整个窗口 ⇒ 一段文本宽度变化就整帧重绘（价格图每 tick
/// 全帧重写的直接原因）。祖先只需要重跑布局，像素变化由 `place_subtree` 精确产出。
#[test]
fn a_deep_layout_change_does_not_damage_the_whole_window() {
    let (_rt, mut app, window) = setup();
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    // 三层容器，最内层是文本。
    let outer = NodeBuilder::boxed().build(tree, root).expect("mount");
    let inner = NodeBuilder::boxed().build(tree, outer).expect("mount");
    let label = NodeBuilder::text("100.00", crate::text::TextStyle::new(12.0))
        .build(tree, inner)
        .expect("mount");
    frame(&mut app, window);
    let _ = app.take_wake_request();

    // 文本内容变长 ⇒ LAYOUT 冒泡到根。
    let tree = app.window_mut(window).expect("window").tree_mut();
    tree.set_text_content(
        label,
        crate::text::TextContent::new("1000000.00", crate::text::TextStyle::new(12.0)),
    );
    frame(&mut app, window);

    let tree = app.window(window).expect("window").tree();
    // 根自己的 damage 必须是空的：它没有移动，也不该为后代的布局变化背 damage。
    assert!(
        tree.get(root).expect("root").damage().is_empty(),
        "根不应因深层布局变化产生 damage（否则整帧重绘）：{:?}",
        tree.get(root).expect("root").damage()
    );
    // 中间容器同样不该有 damage。
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

fn mount_override(app: &mut App, window: WindowId, parent: NodeId, rect: Rect) -> NodeId {
    NodeBuilder::boxed()
        .layout_override(rect)
        .paint(background())
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("mount")
}

fn mount_scroll_container(app: &mut App, window: WindowId, height: f32) -> NodeId {
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(200.0),
        height: taffy::style::Dimension::length(height),
    };
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount")
}

/// P2：同一帧内的多个滚轮事件必须**累加**到动画目标，不能互相覆盖。
///
/// `scroll_to_animated` 每帧只把 offset 推进一步；一次泵迭代里 drain 到的多个滚轮事件
/// 若都以「当前 offset」为基准重算目标，第 1 个事件的 delta 会被静默丢弃（触控板 /
/// 高精度滚轮一个批次常发多个事件，表现为「滚不动」）。
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
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, Size::new(200.0, 400.0))
        .expect("content");

    // 帧内连滚两格（不插帧）：目标必须是 2 行 = 96 px。
    for _ in 0..2 {
        app.dispatch_input(
            window,
            InputEvent::Scroll {
                position: Point::new(50.0, 50.0),
                delta_x: 0.0,
                delta_y: -1.0,
                modifiers: Modifiers::empty(),
            },
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
    assert_eq!(offset, Some(96.0), "两次滚轮必须累加（各 48 px）");
}

/// P2-10：`line_height <= 0` 必须在 `make_scroll` 处拒绝，否则残量计算会写出 NaN。
/// 复审 P2-3：公开入口的 NaN 必须在 API 边界拒绝（否则 `clamp` 保留 NaN，
/// `next == slot.offset` 恒 false ⇒ 每帧自请求重绘）。
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
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    assert!(app.scroll_to(&handle, Point::new(f32::NAN, 0.0)).is_err());
    assert!(
        app.scroll_to(&handle, Point::new(0.0, f32::INFINITY))
            .is_err()
    );
    assert!(
        app.set_scroll_content(&handle, crate::math::Size::new(f32::NAN, 100.0))
            .is_err()
    );
    assert!(
        app.set_scroll_content(&handle, crate::math::Size::new(100.0, f32::INFINITY))
            .is_err()
    );
}

#[test]
fn non_positive_line_height_is_rejected() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    frame(&mut app, window);
    for line_height in [0.0, -48.0, f32::NAN] {
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
/// P0 回归（A4）：`App` 注册的几何必须经 `App::frame` 到达编码器。
///
/// 旧缺陷是「调用点从未把 `App.geometry` 传给编码器」，只测编码器本身抓不到。
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
        bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
        pipeline: crate::paint::geometry::GeometryPipeline::Ui,
    };
    let id = app.geometry_mut().insert(desc).expect("valid geometry");
    let root = app.window(window).expect("window").tree().root();
    NodeBuilder::geometry(id)
        .style({
            let mut style = Style::default();
            style.inner.size = taffy::geometry::Size {
                width: taffy::style::Dimension::length(100.0),
                height: taffy::style::Dimension::length(100.0),
            };
            style
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount geometry");
    frame(&mut app, window);

    let draws = app.window(window).expect("window").encoded_geometry();
    let draw = draws.first().expect("必须编码出一条几何绘制");
    assert_eq!(
        draw.desc.vertex_bytes.len(),
        stride * 3,
        "App 注册的顶点必须真的到达编码器（旧缺陷：编码器持空表 ⇒ 空 desc）"
    );
}

fn lane_quad_bounds(app: &App, window: WindowId, node: NodeId) -> Vec<Rect> {
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

// ── 动作 / 分发 ────────────────────────────────────────────────────────────

#[test]
fn click_dispatches_the_action_and_marks_only_the_target() {
    let (_rt, mut app, window) = setup();
    let calls = Rc::new(Cell::new(0_u32));
    let seen = calls.clone();
    app.on_action(Action::custom("hit"), move |_ctx| seen.set(seen.get() + 1));
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 80.0, 40.0))
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
        InputEvent::PointerDown {
            position: Point::new(20.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );
    let up = app.dispatch_input(
        window,
        InputEvent::PointerUp {
            position: Point::new(20.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );

    assert_eq!(calls.get(), 1, "点击必须分发一次");
    assert_eq!(up.action, Some(Action::custom("hit")));
    assert_eq!(up.target, Some(node));
    assert_eq!(up.click_count, 1);
    assert_eq!(down.hit.map(|hit| hit.node), Some(node));
    // 最小失效：只有 A11Y 脏位（焦点移动），没有 PAINT。
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
        .layout_override(Rect::new(0.0, 0.0, 50.0, 50.0))
        .on_action(Action::custom("activate"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(node)));

    let outcome = app.dispatch_input(
        window,
        InputEvent::Key {
            key: Key::Enter,
            state: KeyState::Down,
            modifiers: Modifiers::empty(),
        },
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
        key: Key::Other(0x53),
        modifiers: Modifiers::CONTROL,
        action: Action::custom("save"),
    });

    let outcome = app.dispatch_input(
        window,
        InputEvent::Key {
            key: Key::Other(0x53),
            state: KeyState::Down,
            modifiers: Modifiers::CONTROL,
        },
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
    // 没有浮层时 Dismiss 是 no-op。
    assert!(!app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
}

// ── 焦点 ──────────────────────────────────────────────────────────────────

#[test]
fn tab_moves_focus_and_marks_a11y_only() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let first = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .on_click(Action::custom("a"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let second = NodeBuilder::boxed()
        .layout_override(Rect::new(60.0, 0.0, 40.0, 40.0))
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
        InputEvent::Key {
            key: Key::Tab,
            state: KeyState::Down,
            modifiers: Modifiers::empty(),
        },
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
        InputEvent::Key {
            key: Key::Tab,
            state: KeyState::Down,
            modifiers: Modifiers::empty(),
        },
    );
    assert_eq!(app.focused(window), Some(second));
    app.dispatch_input(
        window,
        InputEvent::Key {
            key: Key::Tab,
            state: KeyState::Down,
            modifiers: Modifiers::SHIFT,
        },
    );
    assert_eq!(app.focused(window), Some(first), "Shift+Tab 回绕");
}

#[test]
fn focus_ring_marks_paint() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .on_click(Action::custom("a"))
        .focus_ring(FocusRing::new(
            2.0,
            2.0,
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

// ── 滚动（paint 期偏移） ───────────────────────────────────────────────────

/// P2-3.3 回归：内容变短 / 视口变高后必须把滚动偏移重新夹回范围内。
///
/// 偏移只在显式滚动时被夹紧，而可用范围随布局变化。不重夹会留下「滚过内容末尾」的
/// 状态：底部露出空白，且下一次滚轮先吃掉越界量、看起来像「滚不动」。
#[test]
fn layout_change_reclamps_the_scroll_offset() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    // 内容 200 高、视口 150 高 ⇒ 最多滚 50。
    app.scroll_to(&handle, Point::new(0.0, 50.0))
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
        50.0,
        "滚到内容末尾"
    );

    // 内容缩到 80（小于视口）：可用范围变 0 ⇒ 偏移必须回到 0。
    app.set_scroll_content(&handle, Size::new(200.0, 80.0))
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
        0.0,
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
        Rect::new(0.0, 0.0, 200.0, 100.0),
    );
    let _second = mount_override(
        &mut app,
        window,
        container,
        Rect::new(0.0, 100.0, 200.0, 100.0),
    );
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, Size::new(200.0, 200.0))
        .expect("content");
    (rt, app, window, container, child)
}

#[test]
fn text_caret_rect_follows_ancestor_scroll() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let text = NodeBuilder::text("abc", crate::text::TextStyle::new(16.0))
        .layout_override(Rect::new(10.0, 20.0, 100.0, 20.0))
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
    app.scroll_to(&handle, Point::new(0.0, 50.0))
        .expect("scroll");
    frame(&mut app, window);
    let after = app
        .window(window)
        .expect("window")
        .text_caret_rect(text, 3)
        .expect("caret");
    assert!((after.origin.y - before.origin.y + 50.0).abs() < 0.01);
    assert!((after.origin.x - before.origin.x).abs() < 0.01);
}

/// P2-8：窗口变大导致视口变大时，滚动偏移必须在**同一帧**内夹紧。
///
/// 只做帧首夹紧时，这一帧用的是上一帧的视口上限，paint 会把越界的偏移画上屏
/// （内容上移过多 / 底部空白），而下一次输入之前不一定还有帧。
#[test]
fn scroll_offset_is_clamped_in_the_same_frame_as_a_resize() {
    let (_rt, mut app, window) = setup();
    // 百分比尺寸的滚动容器：视口随窗口变化（固定尺寸容器不会触发这条路径）。
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::percent(1.0),
        height: taffy::style::Dimension::percent(1.0),
    };
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    let container = NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount");
    app.window_mut(window)
        .expect("window")
        .set_size(crate::math::Size::new(200.0, 150.0));
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, crate::math::Size::new(200.0, 200.0))
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
    app.scroll_to(&handle, Point::new(0.0, 50.0))
        .expect("scroll");
    frame(&mut app, window);
    assert_eq!(offset(&app), 50.0, "前置条件：视口 150 / 内容 200 ⇒ max 50");

    // 窗口长高到 400 ⇒ 视口 400 > 内容 200 ⇒ max 0：偏移必须在同一帧归零。
    app.window_mut(window)
        .expect("window")
        .set_size(crate::math::Size::new(200.0, 400.0));
    frame(&mut app, window);
    assert_eq!(offset(&app), 0.0, "resize 那一帧就必须夹紧");
}

#[test]
fn scroll_shifts_primitives_without_touching_layout() {
    let (_rt, mut app, window, container, child) = scrollable_app();
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

    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, Point::new(0.0, 50.0))
        .expect("scroll");
    frame(&mut app, window);

    let after = lane_quad_bounds(&app, window, child);
    assert_eq!(after.len(), before.len());
    for (old, new) in before.iter().zip(after.iter()) {
        assert_eq!(new.origin.y, old.origin.y - 50.0, "滚动 N 后图元 y 平移 −N");
        assert_eq!(new.origin.x, old.origin.x);
    }
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
        Point::new(0.0, 50.0)
    );
}

/// P2-4 回归：滚轮必须按 scroll chaining 先给最内层容器，滚不动再交给外层。
///
/// 旧实现只取**一个**最近的滚动祖先：内层到底后滚轮直接失效，外层永远滚不动
/// （嵌套的横向 + 纵向容器、或列表里嵌固定高度面板时最明显）。
#[test]
fn wheel_falls_through_to_the_outer_container_when_the_inner_cannot_scroll() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 150.0);
    // 内层：同样要 `Clip::Bounds` 才是可滚动容器（`make_scroll` 的前置条件）。
    let mut inner_style = Style::default();
    inner_style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(200.0),
        height: taffy::style::Dimension::length(100.0),
    };
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
                line_height: 48.0,
            },
        )
        .expect("outer scrollable");
    app.set_scroll_content(&outer_handle, Size::new(200.0, 400.0))
        .expect("outer content");
    // 内层是**恰好装下内容**的容器：max_offset == 0，它一帧都滚不动。
    let inner_handle = app
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: 48.0,
            },
        )
        .expect("inner scrollable");
    app.set_scroll_content(&inner_handle, Size::new(200.0, 100.0))
        .expect("inner content");
    frame(&mut app, window);

    // 在内层范围内滚一格：内层吃不下，外层必须动。
    app.dispatch_input(
        window,
        InputEvent::Scroll {
            position: Point::new(50.0, 50.0),
            delta_x: 0.0,
            delta_y: -1.0,
            modifiers: Modifiers::empty(),
        },
    );

    // 滚轮走的是动画滚动（`scroll_to_animated`）：把时钟推过动画时长再读回真实偏移。
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
        Some(0.0),
        "内层装得下内容，不得产生偏移"
    );
    assert_eq!(
        outer_offset.map(|offset| offset.y),
        Some(48.0),
        "滚轮必须穿透到能滚的外层容器"
    );
}

#[test]
fn scroll_commits_a_frame() {
    let (_rt, mut app, window, container, _child) = scrollable_app();
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    let before = app.window(window).expect("window").frame_index();
    app.scroll_to(&handle, Point::new(0.0, 30.0))
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
    app.scroll_to(&handle, Point::new(0.0, 10.0))
        .expect("scroll");
    frame(&mut app, window);
    let _ = app.take_wake_request();

    app.scroll_to(&handle, Point::new(0.0, 10.0)).expect("same");

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
        .hit_test(Point::new(50.0, 120.0))
        .map(|hit| hit.local_point);
    assert!(before.is_some(), "滚动前 (50,120) 命中第二个子节点");

    app.scroll_to(&handle, Point::new(0.0, 50.0))
        .expect("scroll");
    let after = app
        .window(window)
        .expect("window")
        .tree()
        .hit_test(Point::new(50.0, 120.0))
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
        .layout_override(Rect::new(0.0, 0.0, 200.0, 80.0))
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
    app.set_scroll_content(&handle, Size::new(200.0, 400.0))
        .expect("content");
    app.scroll_to(&handle, Point::new(0.0, 20.0))
        .expect("scroll");
    frame(&mut app, window);

    assert_eq!(calls.get(), evaluated_once, "滚动不得触发 refresh_styles");
}

// ── 虚拟列表 ──────────────────────────────────────────────────────────────

fn virtual_app() -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
    let (rt, mut app, window) = setup();
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(200.0),
        height: taffy::style::Dimension::length(100.0),
    };
    style.inner.position = taffy::style::Position::Relative;
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

/// P2-6：虚拟列表行必须自动带上「第 N 项 / 共 M 项」，否则屏幕阅读器只播报行内容。
///
/// `VirtualSlot` 是引擎内部状态，应用拿不到 —— 所以 a11y 快照必须由引擎自己填。
#[test]
fn virtual_rows_carry_their_position_in_the_set() {
    let (_rt, mut app, window, container) = virtual_app();
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: 25.0,
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
        // AccessKit `pos_in_set` 是 0-based（适配层 +1）。
        assert_eq!(node.pos_in_set, Some(index), "0-based 位置");
        assert_eq!(node.size_of_set, None, "size 在容器上，不在行上");
        // 经过 adapter 之后 AT 拿到的仍是 0-based（UIA/AT-SPI 自己 +1）。
        let ak = crate::a11y::adapter::to_accesskit(node);
        assert_eq!(ak.position_in_set(), Some(index));
    }
    // 容器节点携带集合总数（consumer 只从祖先读取）。
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
                extent: 25.0,
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
    app.scroll_to(&handle, Point::new(0.0, 250.0))
        .expect("scroll");
    frame(&mut app, window);
    let state = app.virtual_list_state(&list).expect("state");
    assert_eq!((state.first, state.last), (10, 14));
    assert_eq!(state.mounted, 4);
    assert_eq!(built.get(), 8, "只有新进入的 4 行被构建");
}

// ── 浮层 ──────────────────────────────────────────────────────────────────

/// P2-6 回归：`Above` 定位必须把浮层的**下边缘**贴到锚点上方（减掉自身高度）。
///
/// 根节点有 border 时浮层纠位仍须**收敛**且落点是窗口绝对坐标。
///
/// 复审怀疑 taffy 的绝对定位 `inset` 会再扣一次包含块 border（那样每帧都会判「位置
/// 不一致」而重写 inset，永不收敛）。实测：本仓库的 taffy 配置下 `inset` 相对 border
/// box 原点、不额外扣 border —— 写 `(x, y)` 就落在 `(x, y)`，原实现正确。本测试作为
/// 回归保护：防止未来有人「修」成扣 border 而引入 1 px 偏移，或让纠位失去收敛性。
#[test]
fn anchor_placement_converges_with_a_root_border() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    // 布局边框来自 taffy 样式（`PaintSpec.border` 只影响绘制）。
    let mut root_style = Style::default();
    root_style.inner.border = taffy::geometry::Rect {
        left: taffy::style::LengthPercentage::length(1.0),
        right: taffy::style::LengthPercentage::length(1.0),
        top: taffy::style::LengthPercentage::length(1.0),
        bottom: taffy::style::LengthPercentage::length(1.0),
    };
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(root, root_style)
        .expect("root style");
    let anchor = mount_override(&mut app, window, root, Rect::new(100.0, 100.0, 40.0, 20.0));
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
            },
        )
        .expect("overlay");
    let overlay_node = overlay.node();
    let mut style = Style::default();
    style.inner.position = taffy::style::Position::Absolute;
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(60.0),
        height: taffy::style::Dimension::length(30.0),
    };
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
        (bounds.origin.x - 100.0).abs() < 0.5 && (bounds.origin.y - 124.0).abs() < 0.5,
        "浮层落点必须是窗口绝对坐标（Below: y = 100+20+4）：{bounds:?}"
    );

    // 收敛：再出一帧不应再请求新帧（`resolve_overlay_placements` 早退）。
    frame(&mut app, window);
    let _ = app.take_wake_request();
    frame(&mut app, window);
    assert!(
        !app.take_wake_request(),
        "纠位必须收敛，不得每帧都重写 inset"
    );
}

/// 旧实现返回锚点的上边缘，浮层会整个盖住锚点。新建时浮层尺寸未知，`App` 在布局之后
/// 用实测尺寸纠正一次。
#[test]
fn anchored_above_overlay_sits_above_the_anchor() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let anchor = mount_override(&mut app, window, root, Rect::new(100.0, 100.0, 40.0, 20.0));
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
            },
        )
        .expect("overlay");
    let overlay_node = overlay.node();
    let mut style = Style::default();
    style.inner.position = taffy::style::Position::Absolute;
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(60.0),
        height: taffy::style::Dimension::length(30.0),
    };
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(overlay_node, style)
        .expect("style");
    // 布局 → 修正定位 → 再布局（修正会弄脏布局）。
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
        (100.0, 66.0, 30.0),
        "Above：y = 锚点顶 - gap - 自身高度（下边缘贴锚点），实际 {bounds:?}"
    );
}

#[test]
fn overlay_is_last_and_closes_on_outside_click() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let _background = mount_override(&mut app, window, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    frame(&mut app, window);
    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(200.0, 10.0)),
                dismiss: OverlayDismiss::OutsideClick,
            },
        )
        .expect("overlay");
    let overlay_node = overlay.node();
    let mut style = Style::default();
    style.inner.position = taffy::style::Position::Absolute;
    style.inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(200.0),
        top: taffy::style::LengthPercentageAuto::length(10.0),
        right: taffy::style::LengthPercentageAuto::auto(),
        bottom: taffy::style::LengthPercentageAuto::auto(),
    };
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(80.0),
        height: taffy::style::Dimension::length(80.0),
    };
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
            .order()
            .last()
            .copied(),
        Some(overlay_node),
        "浮层必须是 order 的最后一个子树"
    );
    assert_eq!(app.overlays(window).len(), 1);
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .hit_test(Point::new(210.0, 20.0))
            .map(|hit| hit.node),
        Some(overlay_node),
        "浮层优先命中"
    );

    // 点外部 ⇒ 关闭并消费（不再派递给背景）。
    app.dispatch_input(
        window,
        InputEvent::PointerDown {
            position: Point::new(10.0, 10.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
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
                placement: OverlayPlacement::Point(Point::ZERO),
                dismiss: OverlayDismiss::Explicit,
            },
        )
        .expect("overlay");
    let root = root_of(&app, window);

    assert!(app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
    assert!(app.overlays(window).is_empty(), "Dismiss 关 Explicit 浮层");
    assert!(!app.close_overlay(&overlay), "重复关闭返回 false");
}

// ── 拖拽 ──────────────────────────────────────────────────────────────────

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
        .layout_override(Rect::new(100.0, 0.0, 6.0, 100.0))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::PointerDown {
            position: Point::new(102.0, 50.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );
    let state = app.drag_state(window).expect("drag session");
    assert_eq!(state.node(), grip);
    assert_eq!(
        app.take_capture_request(),
        Some((window, true)),
        "拖拽请求指针捕获"
    );

    let moved = app.dispatch_input(
        window,
        InputEvent::PointerMove {
            position: Point::new(122.0, 50.0),
            modifiers: Modifiers::empty(),
        },
    );
    assert_eq!(moved.action, Some(Action::custom("split")));
    assert!(app.drag(&state).is_some_and(|info| info.delta.x == 20.0));

    let up = app.dispatch_input(
        window,
        InputEvent::PointerUp {
            position: Point::new(122.0, 50.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );
    assert_eq!(app.drag_state(window), None);
    assert_eq!(app.take_capture_request(), Some((window, false)));
    assert_eq!(up.action, Some(Action::custom("split")), "抬起派发 End");
    assert_eq!(up.click_count, 0, "拖拽后不产生点击");
    assert_eq!(
        phases.take(),
        vec![DragPhase::Start, DragPhase::Move, DragPhase::End]
    );
}

// ── 动画 ──────────────────────────────────────────────────────────────────

#[test]
fn animation_ticks_write_the_transient_and_end_in_idle() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = mount_override(&mut app, window, root, Rect::new(0.0, 0.0, 40.0, 40.0));
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
        (app.read_transient(&progress) - 0.5).abs() < 1e-3,
        "中点 = 0.5"
    );
    assert!(
        matches!(outcome, FrameOutcome::Committed(_)),
        "注册节点每帧重绘 ⇒ 动画期间必然提交"
    );
    let _ = node;

    app.advance_clock(std::time::Duration::from_millis(60));
    frame(&mut app, window);
    assert_eq!(app.read_transient(&progress), 1.0);
    assert!(!app.animations_running(window), "结束即删槽");
    assert!(!app.cancel_animation(&id), "槽已删");

    app.advance_clock(std::time::Duration::from_millis(100));
    assert!(
        matches!(frame(&mut app, window), FrameOutcome::Idle),
        "动画结束后帧回到 Idle（I3）"
    );
}

// ── 评审 #1…#9 的回归用例 ────────────────────────────────────────────────

#[test]
fn closed_window_returns_unknown_window() {
    let (_rt, mut app, window) = setup();
    let overlay = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::ZERO),
                dismiss: OverlayDismiss::Explicit,
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
                placement: OverlayPlacement::Point(Point::ZERO),
                dismiss: OverlayDismiss::Explicit,
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
    app.scroll_to(&handle, Point::new(0.0, 50.0))
        .expect("scroll");

    // 第二行原本占内容 y = 100..200；滚 50 后屏幕上占 50..150，本地 y = 60 − 50 = 10。
    let hit = app
        .window(window)
        .expect("window")
        .tree()
        .hit_test(Point::new(50.0, 60.0))
        .expect("hit");
    assert_eq!(hit.local_point, Point::new(50.0, 10.0));
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
        .layout_override(Rect::new(0.0, 0.0, 100.0, 100.0))
        .paint(background())
        .on_click(Action::custom("under"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    app.create_overlay(
        window,
        OverlayOptions {
            placement: OverlayPlacement::Point(Point::new(200.0, 10.0)),
            dismiss: OverlayDismiss::OutsideClick,
        },
    )
    .expect("overlay");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::PointerDown {
            position: Point::new(20.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );
    let up = app.dispatch_input(
        window,
        InputEvent::PointerUp {
            position: Point::new(20.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
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
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .on_click(Action::custom("a"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let second = NodeBuilder::boxed()
        .layout_override(Rect::new(60.0, 0.0, 40.0, 40.0))
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
    // Tab 不能卡死：从「无焦点」重新开始，干净地落到第二个节点。
    app.dispatch_input(
        window,
        InputEvent::Key {
            key: Key::Tab,
            state: KeyState::Down,
            modifiers: Modifiers::empty(),
        },
    );
    assert_eq!(app.focused(window), Some(second));
}

#[test]
fn virtual_list_mounts_rows_on_the_first_frame_and_grows_on_resize() {
    let (_rt, mut app, window) = setup();
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(200.0),
        height: taffy::style::Dimension::length(100.0),
    };
    style.inner.position = taffy::style::Position::Relative;
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    let container = NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount");
    // 注意：**先**建列表、再布局（首帧视口还是 0）。
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: 25.0,
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

    // 视口长高 ⇒ 布局后再修窗口（不能停留在旧区间）。
    app.window_mut(window)
        .expect("window")
        .set_size(Size::new(400.0, 300.0));
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style(container, {
            let mut style = Style::default();
            style.inner.size = taffy::geometry::Size {
                width: taffy::style::Dimension::length(200.0),
                height: taffy::style::Dimension::length(200.0),
            };
            style.inner.position = taffy::style::Position::Relative;
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

/// P1-2.2 回归：首帧补挂的行**必须**有真实布局高度。
///
/// 旧实现里 `rewindow_virtuals` 在 `layout_if_dirty` **之后**挂载行，而帧尾
/// `settle_frame` 无条件清掉全树 `LAYOUT` —— 这些行本帧从未参与 taffy 布局，脏位却被清掉，
/// 于是永久停在 0×0、不产生任何图元。`virtual_list_mounts_rows_on_the_first_frame_and_grows_on_resize`
/// 只断言挂载区间（`state.first/last`），所以没暴露这个 bug。
#[test]
fn virtual_rows_are_laid_out_with_the_configured_extent() {
    let (_rt, mut app, window) = setup();
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(200.0),
        height: taffy::style::Dimension::length(100.0),
    };
    style.inner.position = taffy::style::Position::Relative;
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
                extent: 25.0,
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
    // 再出一帧：若首帧把补挂行的 LAYOUT 清掉了，这里也救不回来（脏位已丢）。
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
            bounds.size.height, 25.0,
            "行 {index} 的高度必须等于 extent（实际 {bounds:?}）"
        );
        assert!(
            bounds.size.width > 0.0,
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
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .on_action(Action::custom("activate"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(node)));

    app.dispatch_input(
        window,
        InputEvent::Key {
            key: Key::Enter,
            state: KeyState::Down,
            modifiers: Modifiers::SHIFT | Modifiers::CONTROL,
        },
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
    let node = mount_override(&mut app, window, root, Rect::new(0.0, 0.0, 40.0, 40.0));
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
    assert!((app.read_transient(&value) - 0.5).abs() < 1e-3);

    // 半途改主意：1 → 0。旧槽必须让位（否则两条插值互相覆盖 ⇒ 回弹）。
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
    // 从当前值 0.5 重新起步：走一半 ⇔ 0.5 + (0 − 0.5) × 0.5 = 0.25。
    assert!(
        (app.read_transient(&value) - 0.25).abs() < 1e-3,
        "重定向必须从当前值起步：{}",
        app.read_transient(&value)
    );

    app.advance_clock(std::time::Duration::from_millis(60));
    frame(&mut app, window);
    assert_eq!(app.read_transient(&value), 0.0, "终点是 0，不得回弹到 1");
    assert!(!app.animations_running(window));
}

/// P2-5 回归：系统取消指针会话时，拖拽必须结束并派发 `Cancel`，不能卡在「按住」。
///
/// Windows 的 `WM_CAPTURECHANGED`（Alt+Tab / 系统弹窗 / 别的窗口 `SetCapture`）之后
/// 不会再有 `WM_LBUTTONUP`；平台层把它翻译成 `InputEvent::PointerCancel`。缺失这条路径
/// 时拖拽会话永久存活，用户只能重启应用。
#[test]
fn pointer_cancel_ends_the_drag_without_committing_a_click() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let phases = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = phases.clone();
    let grip = NodeBuilder::boxed()
        .layout_override(Rect::new(100.0, 0.0, 6.0, 100.0))
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
        InputEvent::PointerDown {
            position: Point::new(102.0, 50.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );
    app.dispatch_input(
        window,
        InputEvent::PointerMove {
            position: Point::new(160.0, 50.0),
            modifiers: Modifiers::empty(),
        },
    );
    assert!(app.drag_state(window).is_some(), "拖拽应已开始");

    // 系统夺走捕获：没有 PointerUp，只有取消。
    app.dispatch_input(window, InputEvent::PointerCancel);

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
        .layout_override(Rect::new(100.0, 0.0, 6.0, 100.0))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    app.on_action(Action::custom("split"), |_ctx| {});
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::PointerDown {
            position: Point::new(102.0, 50.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );
    let before = app.drag_state(window).expect("drag");
    app.dispatch_input(
        window,
        InputEvent::PointerDown {
            position: Point::new(300.0, 200.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
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
        .layout_override(Rect::new(10.0, 10.0, 100.0, 100.0))
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
        InputEvent::PointerDown {
            position: Point::new(20.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
    );
    let up = app.dispatch_input(
        window,
        InputEvent::PointerUp {
            position: Point::new(40.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        },
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
        .scroll_to(&handle, Point::new(0.0, 10.0))
        .expect_err("节点已卸载");
    assert!(matches!(error, EngineError::StaleNode { node } if node == container));
}

/// P2-7：IME 启用状态是**每窗口**目标状态，默认启用；关窗时从旁表清掉。
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

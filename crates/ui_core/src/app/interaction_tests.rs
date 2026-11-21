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
            InputEvent::scroll_lines(
                Point::new(50.0, 50.0),
                Point::new(0.0, -1.0),
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
        draw.snapshot.vertex_len() as usize,
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
        InputEvent::pointer_down(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
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

/// **端到端**（生产路径，不是手工构造 `SceneDelta` 的合成场景）：滚动 1 px 的那一帧必须是
/// 「不重建任何 lane + 不重编码 + damage = 视口」，且滚动容器**之外**的固定元素完全不动。
///
/// 复审 AR4 P1-3 的核心验收。接线前 `Window.scroll_slots` 恒空、`LaneSource::lane_scroll` 恒 0，
/// 滚动走的是「重建全部 lane + 重编码 + 全量 damage」——`scroll_1px_10k` 因此是 4 ms 级
/// （目标 ≤200 µs）。这条用例用真实 `App::frame` 路径把三件事一起钉住，undo 掉任一处接线都会失败。
#[cfg(feature = "test-support")]
#[test]
fn a_one_pixel_scroll_frame_rebuilds_nothing_and_damages_only_the_viewport() {
    let (_rt, mut app, window, container, child) = scrollable_app();
    // 固定元素：滚动容器**之外**的兄弟（滚动条 / 工具栏 / 表头都属这一类）。
    let root = app.window(window).expect("window").tree().root();
    let fixed = mount_override(&mut app, window, root, Rect::new(300.0, 0.0, 40.0, 40.0));
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
    app.scroll_to(&handle, Point::new(0.0, 1.0))
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
    // damage = 视口本身；`paint_frame` 对滚动 damage 留了 1 物理像素的抗锯齿外扩。
    let padded = crate::math::PhysicalRect::new(
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
    // 图元字节一个都没动：滚动容器的内容、容器外的固定元素都保持原样。
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
        Point::new(0.0, 1.0),
        "偏移进槽位表"
    );
    // lane 必须**登记**了槽位 1：GPU 侧靠 FrameOp::Instances.scroll 才把偏移加上去；
    // 槽位恒 0 时滚动不会生效，而所有计数仍是 0 —— 所以这条断言必须显式写。
    let view = app.window(window).expect("window");
    let lane = view.tree().lane_of(child).expect("child lane");
    assert_eq!(
        view.scene().lane_scroll(lane),
        1,
        "滚动容器内的 lane 必须登记槽位 1"
    );
}

/// P0 回归：inline composer 在 Idle 帧不得提前消费并丢弃已编码的 lane 实例字节；下一
/// 个滚动提交帧必须能重新编码出非空 payload。
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
        Rect::new(0.0, 900.0, 200.0, 100.0),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, Size::new(200.0, 1100.0))
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

    app.scroll_to(&handle, Point::new(0.0, 900.0))
        .expect("scroll");
    let scroll = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
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

/// P0 回归：连续多个 Idle 帧保留的 delta 必须在一次提交里全部合并（lane 不丢、同槽不重复）。
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
        Rect::new(0.0, 900.0, 200.0, 100.0),
    );
    let fourth = mount_override(
        &mut app,
        window,
        container,
        Rect::new(0.0, 1000.0, 200.0, 100.0),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, Size::new(200.0, 1100.0))
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

    app.scroll_to(&handle, Point::new(0.0, 900.0))
        .expect("scroll");
    let report = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
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

/// P0 回归：Idle 帧保留的 removed 必须活到下一次提交，且同槽旧 lane 不得复活。
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
        Rect::new(0.0, 900.0, 200.0, 100.0),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, Size::new(200.0, 1100.0))
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

/// P0 回归：Idle 帧保留 delta 之后，a11y-only 快路径不得被永久饿死（R13-6）。
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
        Rect::new(0.0, 900.0, 200.0, 100.0),
    );
    let label_signal = app.signal(smol_str::SmolStr::new_static("旧的"));
    let source = label_signal;
    let label = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 100.0, 20.0))
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
    app.set_scroll_content(&handle, Size::new(200.0, 1100.0))
        .expect("content");
    frame(&mut app, window);
    let _ = app.a11y_take(window);
    app.window_mut(window).expect("window").detach_composer();

    // 视口外改色 ⇒ Idle，且 delta 被保留（P0-4 的新不变量）。
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

    // 只写 a11y 绑定：本帧不得跑像素管线，a11y-only 快路径必须照常记一笔。
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

/// P0 回归：Idle 帧构建的 `SceneDelta` 不得被丢弃；滚入视口时必须带上该 lane 的新图元。
///
/// 旧实现 `release_unpublished` 直接 `pending_delta = None`，但 `DeltaBuilder` 的版本号已经
/// 推进到新版本 ⇒ 下一帧 `collect` 认为「没变化」，滚动帧（不重建 lane）只能画出旧图元。
/// 这里把第三行放在滚动视口外：改色帧的 damage 被裁空 ⇒ `FrameOutcome::Idle`；随后滚入
/// 视口，滚动帧本身不重建 lane，唯一能带上新图元的通道就是被保留并合并的旧 delta。
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
        Rect::new(0.0, 900.0, 200.0, 100.0),
    );
    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.set_scroll_content(&handle, Size::new(200.0, 1100.0))
        .expect("content");
    frame(&mut app, window);
    // 模拟真实窗口的 threaded 路径：编码器不在 UI 线程（否则 inline composer 会在 Idle
    // 帧里提前消费 delta，测不到 UI 侧保留逻辑）。渲染器仍保留，用来让 `Plan::Idle` 真正
    // 退回 Idle。
    app.window_mut(window).expect("window").detach_composer();

    // 只改颜色（PAINT-only）：第三行在视口 150 之外，damage 被裁空 ⇒ 本帧必须 Idle。
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

    // 滚到 900：第三行进入视口。纯滚动帧不重建 lane，必须由保留的 delta 带上新颜色。
    app.scroll_to(&handle, Point::new(0.0, 900.0))
        .expect("scroll");
    let report = app
        .window_mut(window)
        .expect("window")
        .paint_frame(
            &crate::paint::geometry_store::GeometryStore::new(),
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

/// **滚动合成端偏移**（架构决策 B3B6 §4 / wave-5）：滚动既不改 `layout.bounds`，也**不改图元**。
///
/// 偏移移动到 `Window` 的滚动槽表里（槽位 → 偏移），合成线程只更新 `FramePayload::scroll`，
/// 绘制期由 GPU 把偏移加到顶点上（`frame.params.yz`）。旧实现把偏移烘焙进图元，于是每滚 1 px
/// 都要重建并重编码全部 lane（`scroll_1px_10k` 因此是 4 ms 级）。
#[test]
fn scrolling_moves_the_offset_into_the_slot_table_without_touching_lanes() {
    let (_rt, mut app, window, container, child) = scrollable_app();
    // `scrollable_app` 的最后一帧发生在 `make_scroll` **之前**：滚动槽位只有跑过一帧 paint 才有。
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
    assert_eq!(slots_before[1].0, Point::ZERO, "初始偏移为 0");

    let handle = app.scroll_handle(window, container).expect("scroll handle");
    app.scroll_to(&handle, Point::new(0.0, 50.0))
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
        Point::new(0.0, 50.0),
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
        InputEvent::scroll_lines(
            Point::new(50.0, 50.0),
            Point::new(0.0, -1.0),
            Modifiers::empty(),
        ),
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
                ..OverlayOptions::default()
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
                ..OverlayOptions::default()
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
                ..OverlayOptions::default()
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
            .hit_test(Point::new(210.0, 20.0))
            .map(|hit| hit.node),
        Some(overlay_node),
        "浮层优先命中"
    );

    // 点外部 ⇒ 关闭并消费（不再派递给背景）。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(10.0, 10.0),
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
                placement: OverlayPlacement::Point(Point::ZERO),
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
        InputEvent::pointer_down(
            Point::new(102.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    // R02-9：按下只登记拖拽候选，越阈值才成立（并同时取得捕获）。
    assert_eq!(app.drag_state(window), None, "未越阈值前不得成立拖拽会话");
    assert!(app.take_capture_requests().is_empty(), "按下不请求指针捕获");

    let moved = app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(122.0, 50.0), Modifiers::empty()),
    );
    let state = app.drag_state(window).expect("drag session");
    assert_eq!(state.node(), grip);
    assert_eq!(
        app.take_capture_requests(),
        vec![(window, true)],
        "拖拽成立时请求指针捕获"
    );
    assert_eq!(moved.action, Some(Action::custom("split")));
    assert!(app.drag(&state).is_some_and(|info| info.delta.x == 20.0));

    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(122.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.drag_state(window), None);
    assert_eq!(app.take_capture_requests(), vec![(window, false)]);
    assert_eq!(up.action, Some(Action::custom("split")), "抬起派发 End");
    assert_eq!(up.click_count, 0, "拖拽后不产生点击");
    // R02-9：`Start` 在越过阈值的那次移动上派发（这就是「拖拽成立」的时刻），之后才是 `Move`。
    // 本例只移动了一次就越过阈值，因此没有 `Move`。
    assert_eq!(phases.take(), vec![DragPhase::Start, DragPhase::End]);
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
                placement: OverlayPlacement::Point(Point::ZERO),
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
            ..OverlayOptions::default()
        },
    )
    .expect("overlay");
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(20.0, 20.0),
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
        InputEvent::key_down(KeyCode::Tab, Modifiers::empty()),
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
    assert!((*app.read_transient(&value) - 0.5).abs() < 1e-3);

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
        (*app.read_transient(&value) - 0.25).abs() < 1e-3,
        "重定向必须从当前值起步：{}",
        app.read_transient(&value)
    );

    app.advance_clock(std::time::Duration::from_millis(60));
    frame(&mut app, window);
    assert_eq!(*app.read_transient(&value), 0.0, "终点是 0，不得回弹到 1");
    assert!(!app.animations_running(window));
}

/// R01-13 回归：零时长动画是「立即到位」，必须与**同一目标**上的长动画互斥。
///
/// 旧实现只在 `duration == 0` 分支里写一次瞬态值就返回，**不清旧槽**：半途「立即置为 0」
/// 之后，上一次注册的 100 ms 动画仍在 `animations` 里，下一帧 `tick_animations` 会把同一个
/// `Transient` 继续推向旧目标值 —— 调用方看到的是「设了 0 ms 却被拉回」。
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

    // 半途改主意：立即到位（0 ms）。
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

    // 再走一段连续帧：旧槽若还活着，会把值推向 1.0。
    app.advance_clock(std::time::Duration::from_millis(200));
    frame(&mut app, window);
    assert_eq!(
        *app.read_transient(&value),
        0.25,
        "旧动画不得在后续帧回写同一个瞬态值"
    );
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
        InputEvent::pointer_down(
            Point::new(102.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(160.0, 50.0), Modifiers::empty()),
    );
    assert!(app.drag_state(window).is_some(), "拖拽应已开始");

    // 系统夺走捕获：没有 PointerUp，只有取消。
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
        .layout_override(Rect::new(100.0, 0.0, 6.0, 100.0))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    app.on_action(Action::custom("split"), |_ctx| {});
    frame(&mut app, window);

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(102.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    // R02-9：拖拽会话在越阈值时才成立。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(130.0, 50.0), Modifiers::empty()),
    );
    let before = app.drag_state(window).expect("drag");
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(300.0, 200.0),
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
        InputEvent::pointer_down(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(40.0, 20.0),
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

// ── R02 输入模型（P0/P1）的端到端回归 ──────────────────────────────────────

/// R02-2：自动重复不触发绑定，也不重复激活焦点控件（长按 Enter 只执行一次）。
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
    // 同一次按住的自动重复：必须被忽略。
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

    // 焦点控件的 Enter 激活同理：重复不重复激活。
    let activations = Rc::new(Cell::new(0_u32));
    let seen = activations.clone();
    app.on_action(Action::custom("activate"), move |_ctx| {
        seen.set(seen.get() + 1)
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

/// R02-3：次键**不**产生 click、**不**抢焦点；主键的 click 携带 button。
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
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .on_click(Action::custom("other"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let target = NodeBuilder::boxed()
        .layout_override(Rect::new(100.0, 0.0, 60.0, 60.0))
        .paint(background())
        .on_click(Action::custom("hit"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(other)));

    // 右键：不派发 click、不改焦点。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(120.0, 20.0),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(120.0, 20.0),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 0, "右键不得触发 on_click");
    assert_eq!(up.click_count, 0);
    assert_eq!(app.focused(window), Some(other), "次键不得抢焦点");
    let _ = target;

    // 主键：派发 click，并且来源里带的是主键。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(120.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(120.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 1);
    assert_eq!(up.click_count, 1);
    assert_eq!(button_source.get(), Some(PointerButton::Primary));
}

/// R02-4：悬停迁移可观测 —— `App::hovered` + `Event::HoverChanged` + 变化节点标 `PAINT`。
#[test]
fn hover_transitions_are_observable_and_mark_paint() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    // `layout_override` 是叶子节点（不能有子节点），容器要用普通样式撑出尺寸。
    let mut outer_style = Style::default();
    outer_style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(100.0),
        height: taffy::style::Dimension::length(100.0),
    };
    let outer = NodeBuilder::boxed()
        .style(outer_style)
        .bind_style(|_| StylePatch::default())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let inner = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 40.0, 40.0))
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
        InputEvent::pointer_move(Point::new(20.0, 20.0), Modifiers::empty()),
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
    // 进入链由外到内，且包含**根与祖先**（CSS `:hover` 的传播语义：子节点悬停时父容器也高亮）。
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

    // 移到空白处：离开链由内到外。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(500.0, 500.0), Modifiers::empty()),
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
    // 离开链由内到外：先是悬停节点，最后是根。
    assert_eq!(left.first().copied(), Some(inner));
    assert_eq!(left.last().copied(), Some(root));
    assert!(left.contains(&outer));
}

/// R02-9：位移未越阈值 ⇒ 是点击，**不**派发 `DragPhase::Start`。
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
        .layout_override(Rect::new(100.0, 0.0, 60.0, 100.0))
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
            Point::new(120.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    // 手抖 1 px：不该开始拖拽。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(121.0, 51.0), Modifiers::empty()),
    );
    assert!(phases.take().is_empty(), "未越阈值不得派发 Drag");
    assert_eq!(app.drag_state(window), None);
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(121.0, 51.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(hits.get(), 1, "未开始拖拽 ⇒ 仍然是点击");
    let _ = grip;
}

/// R02-9：越过阈值才派发 `Start`，delta 是真实位移，并取得指针捕获。
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
            list.push(delta.x);
            seen.set(list);
        }
    });
    let root = root_of(&app, window);
    let grip = NodeBuilder::boxed()
        .layout_override(Rect::new(100.0, 0.0, 60.0, 100.0))
        .on_drag(Action::custom("split"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    let _ = app.take_capture_requests();

    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(120.0, 50.0),
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
        InputEvent::pointer_move(Point::new(124.0, 50.0), Modifiers::empty()),
    );
    assert!(starts.take().is_empty(), "4 px 恰好不越阈值");
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(140.0, 50.0), Modifiers::empty()),
    );
    assert_eq!(starts.take(), vec![20.0], "Start 必须带真实位移");
    assert_eq!(app.take_capture_requests(), vec![(window, true)]);
    let _ = grip;
}

/// R02-8：指针捕获是公开 API，生命周期可观测。
#[test]
fn pointer_capture_api_has_an_explicit_lifecycle() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);

    let stale = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
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

/// R02-16：`KeyState::Up` 落地 + 按住查询；焦点容器上的方向键默认滚动。
#[test]
fn key_up_lands_and_arrow_keys_scroll_the_focused_container() {
    let (_rt, mut app, window) = setup();
    let container = scrollable_container(&mut app, window);
    let button = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
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
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, Size::new(200.0, 400.0))
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
        48.0,
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

    // PageDown 一页（视口 150，内容 400 ⇒ max 250，夹到 250 之前是 150）。
    app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::PageDown, Modifiers::empty()),
    );
    assert_eq!(app.scroll_state(&handle).expect("state").offset.y, 148.0);

    // 应用可以用自己的绑定拦截默认行为。
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

/// R02-12：模态浮层的焦点陷阱 + 关闭后焦点恢复。
#[test]
fn a_modal_overlay_traps_focus_and_restores_it_on_close() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let opener = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .on_click(Action::custom("opener"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let background = NodeBuilder::boxed()
        .layout_override(Rect::new(50.0, 0.0, 40.0, 40.0))
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
                placement: OverlayPlacement::Point(Point::new(100.0, 100.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("overlay");
    let first = NodeBuilder::boxed()
        .layout_override(Rect::new(110.0, 110.0, 40.0, 20.0))
        .on_click(Action::custom("first"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            overlay.node(),
        )
        .expect("mount");
    let second = NodeBuilder::boxed()
        .layout_override(Rect::new(160.0, 110.0, 40.0, 20.0))
        .on_click(Action::custom("second"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            overlay.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    // 作用域外不可聚焦。
    assert!(
        !app.set_focus(window, Some(background)),
        "模态外节点不可聚焦"
    );
    // R02-12：作用域外的焦点视为无焦点，并在下一次输入时移入模态内第一个可聚焦节点。
    assert_eq!(
        app.focused(window),
        None,
        "模态打开后作用域外的焦点不得再被报告"
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(5.0, 5.0), Modifiers::empty()),
    );
    assert_eq!(
        app.focused(window),
        Some(first),
        "焦点修复：移入模态内第一个可聚焦节点"
    );
    // Tab 只在浮层内循环（从修复后的 first 出发）。
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

    // 关闭后焦点还给打开者。
    assert!(app.close_overlay(&overlay));
    assert_eq!(
        app.focused(window),
        Some(opener),
        "关闭必须恢复 opener 焦点"
    );
}

/// R02-5：`App::events` 有界（超限丢最旧并计数）。
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

/// R02-10：`unit = Pixel` 立即到位（不建动画）；`unit = Line` 走平滑动画。
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
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, Size::new(200.0, 400.0))
        .expect("content");
    frame(&mut app, window);

    // 行单位：登记动画，位移在本帧还看不到。
    app.dispatch_input(
        window,
        InputEvent::scroll_lines(
            Point::new(50.0, 50.0),
            Point::new(0.0, -1.0),
            Modifiers::empty(),
        ),
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        0.0,
        "行单位走动画（同帧内还没动）"
    );

    // 像素单位：1:1 立即跟随（触控板手感）。
    app.dispatch_input(
        window,
        InputEvent::Scroll {
            position: Point::new(50.0, 50.0),
            delta: Point::new(0.0, -30.0),
            unit: ScrollUnit::Pixel,
            phase: ScrollPhase::Update,
            modifiers: Modifiers::empty(),
            timestamp: std::time::Instant::now(),
        },
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        30.0,
        "像素单位必须立即到位"
    );
}

/// R02-14：连击判定用**事件时间戳**，而不是分发时刻。
#[test]
fn click_count_uses_the_event_time_stamp() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let node = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 100.0, 100.0))
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
                Point::new(20.0, 20.0),
                PointerButton::Primary,
                Modifiers::empty(),
            )
            .with_timestamp(at),
        );
        app.dispatch_input(
            window,
            InputEvent::pointer_up(
                Point::new(20.0, 20.0),
                PointerButton::Primary,
                Modifiers::empty(),
            )
            .with_timestamp(at),
        )
    };
    assert_eq!(click(&mut app, base).click_count, 1);
    // 100 ms 之后：仍是双击的第二个。
    assert_eq!(
        click(&mut app, base + std::time::Duration::from_millis(100)).click_count,
        2
    );
    // 事件时间戳间隔 1 s（超过阈值，即使分发几乎同时）：不得算成双击。
    assert_eq!(
        click(&mut app, base + std::time::Duration::from_millis(1_100)).click_count,
        1,
        "阈值必须按事件时刻判定，不是分发时刻"
    );
    let _ = node;
}

/// 建一个可滚动容器（100×100 视口），不带内容。
fn scrollable_container(app: &mut App, window: WindowId) -> NodeId {
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(100.0),
        height: taffy::style::Dimension::length(100.0),
    };
    style.clip = Clip::Bounds;
    let tree = app.window_mut(window).expect("window").tree_mut();
    let root = tree.root();
    NodeBuilder::boxed()
        .style(style)
        .build(tree, root)
        .expect("mount")
}

// ── AR1 返工回归（wave-2） ────────────────────────────────────────────────

/// R02-12（P1 回归，AR1 探针 1）：**模态之上的弹出层点击必须命中**。
///
/// 失败面：`create_overlay` 恒把浮层挂 root，旧判据只认 `is_in_subtree(hit, scope)` ⇒ 对话框里
/// 的下拉/菜单必然落在作用域外，按下被整体 `return`，菜单项永远点不动（实测 click=0）。
/// 正确语义：浮层的 `opener` 在模态内（可嵌套递归）⇒ 该浮层仍在作用域内。
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
        .layout_override(Rect::new(0.0, 0.0, 400.0, 300.0))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(background)));

    // 模态对话框（子节点在 `create_overlay` 返回后才挂载）。
    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(20.0, 20.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let confirm = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 80.0, 24.0))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    // 焦点修复：作用域外的焦点在下一次输入时移入模态内第一个可聚焦节点。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(5.0, 5.0), Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(confirm), "焦点必须移入模态");

    // 对话框里的下拉菜单：浮层挂 root，opener = 模态内的按钮。
    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(120.0, 60.0)),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let item = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 100.0, 20.0))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    // 菜单项屏幕位置 = 浮层原点 (120, 60) + 子节点 (10, 10) = (130, 70)。
    let position = Point::new(140.0, 78.0);
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

/// AR1b N1a（P2 回归，复审探针 6）：**同一个回调**里先开模态、紧接着开它的下拉菜单。
///
/// 失败面：`opener` 取自 `App::focused`（被模态作用域过滤）⇒ 模态刚建立、焦点还没被
/// `validate_focus` 修复时 `focused()` 返回 `None` ⇒ 菜单 `opener = None` ⇒
/// `in_modal_scope` 直接判「作用域外」：菜单项命中正确但按下被整体消费（`click_count == 0`）、
/// 动作不派发，浮层只能被应用重建。
///
/// 本用例**故意不**先派发一次输入去修焦点 —— 那正是被绕过的时机。焦点留在背景按钮上，
/// 新求值顺序先按「作用域内的候选」过滤，把它排除后落到模态本身（top-layer 语义）。
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
        .layout_override(Rect::new(0.0, 0.0, 200.0, 100.0))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(background_button)));

    // 同一个回调：先建模态，紧接着开它的下拉菜单（中间没有任何输入事件）。
    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(20.0, 20.0)),
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
                placement: OverlayPlacement::Point(Point::new(120.0, 60.0)),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let confirm = NodeBuilder::boxed()
        .layout_override(Rect::new(30.0, 30.0, 60.0, 24.0))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    let item = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 100.0, 20.0))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    // 时机：模态已建立，背景焦点还没被修复 ⇒ 读取面是 `None`（这正是 opener 被吞的时刻）。
    assert_eq!(
        app.focused(window),
        None,
        "模态打开后作用域外的焦点视为无焦点"
    );

    // 菜单项屏幕位置 = 浮层原点 (120, 60) + 子节点 (10, 10) = (130, 70)。
    let position = Point::new(140.0, 78.0);
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

/// AR1b N1a 的对照分支：**完全没有焦点**时同一个回调里连开模态 + 菜单。
///
/// `opener` 的候选链此时全部为空，最终退化到「当前作用域根」（模态本身）。失败面与上一个
/// 用例相同（复审探针 6 的原始观察值 `click_count == 0`）。
#[test]
fn a_popup_opened_without_any_focus_still_receives_clicks() {
    let (_rt, mut app, window) = setup();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = clicks.clone();
    app.on_action(Action::custom("menu.item"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let root = root_of(&app, window);
    // 纯布局容器：不可聚焦，因此整个窗口**没有焦点**。
    let _background = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 200.0, 100.0))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert_eq!(app.focused(window), None, "前置：没有焦点");

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(20.0, 20.0)),
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
                placement: OverlayPlacement::Point(Point::new(120.0, 60.0)),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let _confirm = NodeBuilder::boxed()
        .layout_override(Rect::new(30.0, 30.0, 60.0, 24.0))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    let item = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 100.0, 20.0))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    let position = Point::new(140.0, 78.0);
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

/// AR1b N1b（P2 回归，复审探针 7）：**完全没有焦点**时打开模态，Enter 必须落到模态内。
///
/// 失败面：`validate_focus` 只在「有焦点但在作用域外」时修复，`focused() == None` 直接返回
/// ⇒ 从不可聚焦内容（图表区 / 自绘画布）打开对话框后，按 Enter 的 `outcome.target` 是
/// `None`，必须先按 Tab 或点一下才有落点。
#[test]
fn a_modal_opened_without_any_focus_fills_the_first_focusable_node() {
    let (_rt, mut app, window) = setup();
    let activations = Rc::new(Cell::new(0_u32));
    let seen = activations.clone();
    app.on_action(Action::custom("modal.ok"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let root = root_of(&app, window);
    // 不可聚焦的图表区：只有布局，没有动作 / `interactive`。
    let chart = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 200.0, 100.0))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    // 用户点了一下图表区：没有可聚焦候选 ⇒ 焦点保持 `None`。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(10.0, 10.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(10.0, 10.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.focused(window), None, "前置：图表区不抢焦点");
    let _ = chart;

    // 应用从图表区的回调里打开对话框。
    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(40.0, 40.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let ok = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 60.0, 24.0))
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

    // 第一次输入就修复焦点并激活模态内第一个可聚焦节点。
    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::Enter, Modifiers::empty()),
    );
    assert_eq!(app.focused(window), Some(ok), "Enter 前焦点必须已移入模态");
    assert_eq!(outcome.target, Some(ok), "Enter 必须有落点");
    assert_eq!(activations.get(), 1);
}

/// R02-12（P1 回归，AR1 探针 2）：**模态外按钮收不到 Enter 激活**。
///
/// 失败面：Enter/Space 激活直接取 `app.focused()` 且不查作用域 ⇒ 焦点留在背景时按 Enter
/// 仍会触发背景按钮（实测 activations=1）。a11y 的 `ActionRequest` 走 `dispatch_action`，
/// 同一路径也必须被作用域拦住。
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
        .layout_override(Rect::new(0.0, 0.0, 100.0, 40.0))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(background)));

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(200.0, 100.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                restore_focus: true,
                a11y: crate::a11y::A11yOverride::default(),
            },
        )
        .expect("modal");
    let ok = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 60.0, 24.0))
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

    // 程序化 / a11y 分发同样受作用域约束。
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
    // 作用域内的程序化动作照常。
    assert!(app.dispatch_action(
        window,
        ok,
        Action::custom("modal.ok"),
        ActionSource::Programmatic
    ));
    assert_eq!(modal_hits.get(), 2);

    // 引擎级动作不受作用域限制：模态打开时 `Dismiss` 仍能关掉它（Esc 走同一语义）。
    let root = root_of(&app, window);
    assert!(app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
    assert!(app.overlays(window).is_empty(), "Dismiss 必须关掉模态");
}

/// R02-8（P1 回归，AR1 探针 3）：捕获目标被卸载后必须**释放捕获并派发 `PointerCancel`**。
///
/// 失败面：`remove_node` 只做焦点恢复，`pointer_captured()` 不校验存活 ⇒ 永久返回已卸载
/// `NodeId`，平台 `SetCapture` 不释放，`on_pointer_leave` 因 `capture.is_some()` 保留按下记录。
#[test]
fn removing_the_captured_node_releases_the_capture() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    // 普通容器（可以挂子节点；`LayoutOverride` 节点是叶子，不能有子节点）。
    let panel = NodeBuilder::boxed()
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    let handle = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 20.0, 20.0))
        .build(app.window_mut(window).expect("window").tree_mut(), panel)
        .expect("mount");
    frame(&mut app, window);
    assert!(app.capture_pointer(window, handle));
    assert_eq!(app.pointer_captured(window), Some(handle));
    let _ = app.take_capture_requests();
    let _ = app.drain_events();

    // 删掉**祖先**：捕获目标随之卸载。
    assert!(app.remove_node(window, panel));
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

/// R02-10（P1 回归，AR1 探针 4）：键盘/程序化滚动必须**取消残留的滚轮动画**。
///
/// 失败面：`scroll_to` 不取消 `AnimationTarget::Scroll` ⇒ 滚轮动画在后续帧继续插值，把
/// 键盘 `End`（300.0）拉回动画目标（16.75），画面「回弹」。
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
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, Size::new(100.0, 400.0))
        .expect("content");
    frame(&mut app, window);

    // 离散滚轮：登记 120 ms 平滑动画（同帧 offset 还不动）。
    app.dispatch_input(
        window,
        InputEvent::scroll_lines(
            Point::new(50.0, 50.0),
            Point::new(0.0, -1.0),
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.scroll_state(&handle).expect("state").offset.y, 0.0);
    assert!(app.animations_running(window), "滚轮动画已登记");

    // 程序化跳到底（键盘 End 也走这条：`scroll_by → scroll_to`）。
    app.scroll_to(&handle, Point::new(0.0, 300.0))
        .expect("scroll");
    assert_eq!(app.scroll_state(&handle).expect("state").offset.y, 300.0);
    assert!(
        !app.animations_running(window),
        "scroll_to 必须取消同槽动画"
    );

    // 后续帧不得把偏移拉回。
    app.advance_clock(std::time::Duration::from_millis(200));
    frame(&mut app, window);
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        300.0,
        "残留动画不得回弹"
    );
}

/// R02-16：按住集合是**每窗口**的；关窗必须清理该窗口的集合。
#[test]
fn held_keys_are_per_window_and_cleared_on_close() {
    let (_rt, mut app, window) = setup();
    let other = app.create_window(WindowOptions {
        title: "other".into(),
        size: Size::new(200.0, 200.0),
        scale: Scale::ONE,
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

/// R02-3 / R02-9：**只有主键**驱动拖拽 —— 次键不登记候选，次键抬起也不结束主键的拖拽。
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
        .layout_override(Rect::new(0.0, 0.0, 100.0, 100.0))
        .on_drag(Action::custom("grip"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);

    // 次键按下 + 越阈值移动：不得成立拖拽会话。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(10.0, 10.0),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(40.0, 10.0), Modifiers::empty()),
    );
    assert_eq!(app.drag_state(window), None, "次键不得启动拖拽");
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(40.0, 10.0),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );

    // 主键：越阈值成立。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(10.0, 10.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(40.0, 10.0), Modifiers::empty()),
    );
    assert_eq!(
        app.drag_state(window).map(|state| state.node()),
        Some(grip),
        "主键必须启动拖拽"
    );

    // 次键抬起不得结束会话。
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(40.0, 10.0),
            PointerButton::Secondary,
            Modifiers::empty(),
        ),
    );
    assert!(
        app.drag_state(window).is_some(),
        "次键抬起不得结束主键的拖拽"
    );
    assert_eq!(phases.take(), vec![DragPhase::Start]);

    // 主键抬起：结束（派发 End）。
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(40.0, 10.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(app.drag_state(window), None);
    assert_eq!(phases.take(), vec![DragPhase::End]);
}

/// R02-11（文档化的语义，回归钉住）：二维 `delta` 按轴拆分，沿滚动链逐层消费。
///
/// 内层 `Horizontal` + 外层 `Vertical` = 两轴同时平移（K 线图/深度图的 2D 手势表达方式）。
/// 引擎的容器是单轴的，这里同时钉住「另一轴不被内层吃掉、原样交给外层」。
#[test]
fn a_two_dimensional_wheel_delta_is_split_across_the_scroll_chain() {
    let (_rt, mut app, window) = setup();
    let outer = mount_scroll_container(&mut app, window, 100.0);
    // 内层：嵌在外层里的横向容器（视口 100×50）。
    let mut inner_style = Style::default();
    inner_style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(100.0),
        height: taffy::style::Dimension::length(50.0),
    };
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
                line_height: 48.0,
            },
        )
        .expect("outer scrollable");
    app.set_scroll_content(&outer_handle, Size::new(200.0, 400.0))
        .expect("outer content");
    let inner_handle = app
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Horizontal,
                line_height: 48.0,
            },
        )
        .expect("inner scrollable");
    app.set_scroll_content(&inner_handle, Size::new(400.0, 50.0))
        .expect("inner content");
    frame(&mut app, window);

    // 二维像素 delta：横向 -12（内层吃）、纵向 -3（内层不吃，传给外层）。
    app.dispatch_input(
        window,
        InputEvent::Scroll {
            position: Point::new(20.0, 20.0),
            delta: Point::new(-12.0, -3.0),
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
        12.0,
        "内层横向容器消费 dx"
    );
    assert_eq!(
        app.scroll_state(&outer_handle)
            .expect("outer state")
            .offset
            .y,
        3.0,
        "纵向分量必须传给外层容器"
    );
}

// ── E12：浮层 a11y 角色 + 模态作用域 ─────────────────────────────────────

/// E12：浮层包装节点的 a11y 角色来自**显式声明**，`App::modal_scope` 是可查询的模态作用域。
///
/// 旧实现把每个浮层包装节点自动推成 `Dialog`（并带 Expand/Collapse 死动作），菜单 / tooltip
/// 会被 AT 读成对话框；而且应用无法在建节点后覆写（`Tree::set_a11y_patch` 是 crate 内 API）。
/// 现在：无声明 ⇒ `GenericContainer`；声明 Menu / Dialog 原样生效；模态状态由 `trap_focus`
/// 表达（包装的 `aria-modal` 跟它走，不再跟角色走）。
#[test]
fn an_overlay_a11y_role_is_declared_explicitly_and_the_modal_scope_is_queryable() {
    let (_rt, mut app, window) = setup();
    frame(&mut app, window);
    assert_eq!(app.modal_scope(window), None, "没有模态浮层");

    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(120.0, 60.0)),
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
                placement: OverlayPlacement::Point(Point::new(20.0, 20.0)),
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
        .layout_override(Rect::new(10.0, 10.0, 80.0, 24.0))
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

    // 关闭模态：作用域消失、背景恢复可见。
    assert!(app.close_overlay(&modal));
    assert_eq!(app.modal_scope(window), None);
    frame(&mut app, window);
    let snapshot = app.a11y_take(window).expect("模态关闭必须交付 a11y 增量");
    assert!(
        !snapshot.node_for(menu.node()).expect("菜单").hidden,
        "模态关闭后背景必须重新可见"
    );
}

/// E12：模态打开时背景 hover 被过滤 —— 背景 tooltip **不可能**在模态之上打开（P2-4b R07）。
///
/// 旧 `BlockMouse` 的 hitbox 同时挡命中与 hover；引擎模态作用域必须给出同一语义。这里钉住：
/// - 模态**打开瞬间**，作用域外的悬停立即结束（派生 `HoverChanged(None)`，不需要动鼠标）；
/// - 指针在背景上移动不会重新产生悬停；
/// - 模态子树（以及属于该模态的弹出层）的悬停照常。
#[test]
fn a_modal_filters_background_hover_so_tooltips_cannot_open_over_it() {
    let (_rt, mut app, window) = setup();
    let root = root_of(&app, window);
    let trigger = mount_override(&mut app, window, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    frame(&mut app, window);
    let _ = app.drain_events();

    // 前置：没有模态时，背景触发器正常悬停（tooltip 的输入就是这个）。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(50.0, 50.0), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), Some(trigger));
    let _ = app.drain_events();

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(200.0, 0.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                ..OverlayOptions::default()
            },
        )
        .expect("modal");
    let confirm = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 80.0, 40.0))
        .on_click(Action::custom("modal.confirm"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    // 模态打开 ⇒ 作用域外的悬停立即结束（不需要 PointerMove）。
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

    // 指针在背景上移动：仍然没有悬停（背景 `:hover` 不得亮）。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(60.0, 60.0), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), None);
    assert!(
        app.drain_events()
            .into_iter()
            .all(|(_, event)| !matches!(event, Event::HoverChanged { .. })),
        "背景被模态挡住时不得产生悬停迁移事件"
    );

    // 模态子树里的悬停照常（对话框按钮仍要有 :hover）。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(215.0, 15.0), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), Some(confirm), "模态内部的悬停必须正常");

    // 对话框里的菜单（opener 在模态内）也属于作用域：悬停照常。
    let menu = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(300.0, 100.0)),
                dismiss: OverlayDismiss::OutsideClick,
                ..OverlayOptions::default()
            },
        )
        .expect("menu");
    let item = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 100.0, 20.0))
        .on_click(Action::custom("menu.item"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            menu.node(),
        )
        .expect("mount");
    frame(&mut app, window);
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(320.0, 118.0), Modifiers::empty()),
    );
    assert_eq!(
        app.hovered(window),
        Some(item),
        "属于模态的弹出层必须可悬停（对话框里的下拉菜单）"
    );
}

/// E12：模态挡住背景点击，但 Esc / 焦点陷阱 / 关闭后的交互恢复保持不变。
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
        .layout_override(Rect::new(0.0, 0.0, 100.0, 100.0))
        .on_click(Action::custom("background"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);
    // 模态打开前焦点在背景（模态打开瞬间背景仍是 raw focus）。
    assert!(app.set_focus(window, Some(background)));

    let modal = app
        .create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(150.0, 50.0)),
                dismiss: OverlayDismiss::Explicit,
                trap_focus: true,
                ..OverlayOptions::default()
            },
        )
        .expect("modal");
    let ok = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 60.0, 24.0))
        .on_click(Action::custom("modal.ok"))
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            modal.node(),
        )
        .expect("mount");
    frame(&mut app, window);

    // a11y 焦点（E12）：模态作用域外的 raw 焦点不得作为 `TreeUpdate.focus` 发给 AT ——
    // 背景已经被标 hidden，AT 焦点停在那里与 inert 语义自相矛盾。
    let snapshot = app.a11y_take(window).expect("a11y 交付");
    assert_ne!(
        snapshot.focus_node().engine,
        background,
        "作用域外的背景不得成为 a11y 焦点"
    );

    // 焦点陷阱：作用域外节点不得被设为焦点；焦点移动落在模态内。
    assert!(
        !app.set_focus(window, Some(background)),
        "模态作用域外的节点不得获得焦点"
    );
    app.focus_next(window);
    assert_eq!(app.focused(window), Some(ok), "Tab 必须落在模态内");

    // 背景点击被整体消费（不派发任何动作）。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(50.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(50.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 0, "模态作用域外的点击不得派发");

    // 模态内点击正常。
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(170.0, 70.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(170.0, 70.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(ok_clicks.get(), 1, "模态内的点击必须正常派发");

    // Esc（`Dismiss`）仍然可关模态：作用域只约束「激活节点」，引擎级动作不受限。
    assert!(app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
    assert_eq!(app.modal_scope(window), None, "Esc/Dismiss 必须关掉模态");

    // 关闭后背景恢复：hover 与点击都回来了。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(50.0, 50.0), Modifiers::empty()),
    );
    assert_eq!(app.hovered(window), Some(background));
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(50.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_up(
            Point::new(50.0, 50.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    assert_eq!(clicks.get(), 1, "模态关闭后背景点击必须恢复");
}

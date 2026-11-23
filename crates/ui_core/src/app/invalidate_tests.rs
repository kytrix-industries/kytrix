use super::App;
use crate::{
    error::EngineError,
    math::{Rect, Scale, Size},
    node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
    paint::{
        color::Color,
        style::{PaintSpec, Style, StylePatch},
    },
    window::{WindowId, WindowOptions},
};

fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    let window = app.create_window(WindowOptions {
        title: "r01-16".into(),
        size: Size::new(400.0, 300.0),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    (rt, app, window)
}

fn frame(app: &mut App, window: WindowId) {
    app.frame(window)
        .expect("window exists")
        .expect("frame succeeds");
}

fn paint() -> PaintSpec {
    PaintSpec {
        background: Color::from_rgba8([40, 120, 200, 255]),
        ..PaintSpec::default()
    }
}

fn positioned(rect: Rect) -> Style {
    let mut style = Style::default();
    style.inner.position = taffy::style::Position::Absolute;
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(rect.size.width),
        height: taffy::style::Dimension::length(rect.size.height),
    };
    style.inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(rect.origin.x),
        right: taffy::style::LengthPercentageAuto::length(0.0),
        top: taffy::style::LengthPercentageAuto::length(rect.origin.y),
        bottom: taffy::style::LengthPercentageAuto::length(0.0),
    };
    style
}

fn mount(app: &mut App, window: WindowId, parent: NodeId, rect: Rect) -> NodeId {
    NodeBuilder::boxed()
        .style(positioned(rect))
        .paint(paint())
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("mount")
}

fn dirty(app: &App, window: WindowId, node: NodeId) -> DirtyFlags {
    app.window(window)
        .expect("window")
        .tree()
        .get(node)
        .expect("node alive")
        .dirty()
}

struct Fixture {
    root: NodeId,
    panel: NodeId,
    node: NodeId,
    child_a: NodeId,
    child_b: NodeId,
    sibling: NodeId,
}

fn fixture(app: &mut App, window: WindowId) -> Fixture {
    let root = app.window(window).expect("window").tree().root();
    let panel = mount(app, window, root, Rect::new(0.0, 0.0, 200.0, 200.0));
    let node = mount(app, window, panel, Rect::new(10.0, 10.0, 80.0, 80.0));
    let child_a = mount(app, window, node, Rect::new(12.0, 12.0, 20.0, 20.0));
    let child_b = mount(app, window, node, Rect::new(40.0, 40.0, 20.0, 20.0));
    let sibling = mount(app, window, panel, Rect::new(120.0, 10.0, 40.0, 40.0));
    Fixture {
        root,
        panel,
        node,
        child_a,
        child_b,
        sibling,
    }
}

fn warm(app: &mut App, window: WindowId) -> Fixture {
    let fixture = fixture(app, window);
    frame(app, window);
    let _ = app.take_wake_request();
    fixture
}

#[test]
fn node_repaint_marks_only_the_subtree_and_requests_one_frame() {
    let (_rt, mut app, window) = setup();
    let Fixture {
        root,
        panel,
        node,
        child_a,
        child_b,
        sibling,
    } = warm(&mut app, window);
    assert!(
        dirty(&app, window, panel).contains(DirtyFlags::REUSABLE),
        "前置条件：暖帧后祖先可复用"
    );

    let requests = app.frame_requests();
    assert!(
        app.request_node_repaint(window, node).expect("repaint"),
        "可见子树的请求必须产生帧"
    );
    assert_eq!(app.frame_requests(), requests + 1, "只唤醒一次");
    assert!(app.take_wake_request(), "必须置帧请求位");

    for subtree in [node, child_a, child_b] {
        let flags = dirty(&app, window, subtree);
        assert!(flags.contains(DirtyFlags::PAINT), "子树节点必须标 PAINT");
        assert!(
            !flags.contains(DirtyFlags::REUSABLE),
            "子树节点的 lane 必须失效"
        );
    }
    for outside in [root, panel, sibling] {
        let flags = dirty(&app, window, outside);
        assert!(
            !flags.contains(DirtyFlags::PAINT),
            "子树外的节点不得被标 PAINT（祖先链只做可见性校验）"
        );
        assert!(
            flags.contains(DirtyFlags::REUSABLE),
            "子树外的节点 lane 必须仍可复用"
        );
    }
}

#[cfg(feature = "test-support")]
#[test]
fn node_repaint_rebuilds_only_subtree_lanes_and_stays_within_the_node() {
    let (_rt, mut app, window) = setup();
    let Fixture {
        node,
        child_a,
        child_b,
        ..
    } = warm(&mut app, window);

    assert!(app.request_node_repaint(window, node).expect("repaint"));
    frame(&mut app, window);

    let record = app
        .window(window)
        .expect("window")
        .record()
        .expect("commit 帧必须有记录");
    assert_eq!(
        record.dirty_nodes,
        vec![node, child_a, child_b],
        "只重建子树的 lane（DFS 序）"
    );
    assert_eq!(record.stats.painted_lanes, 3, "重建 3 条 lane");
    assert!(!record.damage_full, "节点级重绘不得退化成整窗");
    assert_eq!(
        record.damage,
        vec![crate::math::PhysicalRect::new(9, 9, 91, 91)],
        "damage = node 盒 (10,10,80,80) + 1 px AA 外扩"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn shrinking_text_damage_covers_the_old_glyphs_outside_the_node_box() {
    let (_rt, mut app, window) = setup();
    let root = app.window(window).expect("window").tree().root();
    let node = {
        let w = app.window_mut(window).expect("window");
        NodeBuilder::text(
            "WWWWWWWWWWWWWW",
            crate::text::TextStyle::new(16.0).with_wrap(crate::text::TextWrap::NoWrap),
        )
        .style(positioned(Rect::new(20.0, 20.0, 60.0, 20.0)))
        .build(w.tree_mut(), root)
        .expect("文本挂载成功")
    };
    frame(&mut app, window);
    let old_bounds = {
        let tree = app.window(window).expect("window").tree();
        let target = tree.get(node).expect("node");
        let origin = target.layout().bounds.origin;
        let content = tree.content_offset(node);
        let line = tree.text_line(node).expect("首帧必须已经 shape");
        let mut bounds = Rect::default();
        for run in line.glyph_runs() {
            for glyph in &run.glyphs {
                bounds = bounds.union(Rect::from_origin_size(
                    crate::math::Point::new(
                        origin.x + content.x + glyph.position.x,
                        origin.y + content.y + glyph.position.y,
                    ),
                    glyph.size,
                ));
            }
        }
        bounds
    };
    let node_bounds = app
        .window(window)
        .expect("window")
        .tree()
        .get(node)
        .expect("node")
        .layout()
        .bounds;
    assert!(
        old_bounds.origin.x + old_bounds.size.width > node_bounds.origin.x + 100.0,
        "前置条件：14 个 W 必须显著溢出 60 px 节点盒（旧字形 {old_bounds:?}）"
    );

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_text_content(
            node,
            crate::text::TextContent::new(
                "i",
                crate::text::TextStyle::new(16.0).with_wrap(crate::text::TextWrap::NoWrap),
            ),
        );
    assert!(
        app.request_node_repaint(window, node).expect("repaint"),
        "内容变更必须请求一帧"
    );
    frame(&mut app, window);

    let record = app
        .window(window)
        .expect("window")
        .record()
        .expect("commit 帧必须有记录");
    assert!(!record.damage_full, "内容收缩不得退化成整窗 damage");
    let last_pixel_x = (old_bounds.origin.x + old_bounds.size.width - 1.0).floor() as i32;
    let sample_y = (old_bounds.origin.y + old_bounds.size.height / 2.0).floor() as i32;
    assert!(
        record.damage.iter().any(|rect| {
            rect.x0 <= last_pixel_x
                && last_pixel_x < rect.x1
                && rect.y0 <= sample_y
                && sample_y < rect.y1
        }),
        "damage 必须覆盖旧字形的溢出部分（旧字形右端 x≈{last_pixel_x}, y={sample_y}）；\
         旧实现只覆盖节点盒 ⇒ 残影。damage={:?}",
        record.damage
    );
}

#[test]
fn node_repaint_under_a_hidden_ancestor_is_a_noop() {
    let (_rt, mut app, window) = setup();
    let Fixture { panel, node, .. } = warm(&mut app, window);
    let _ = app.take_wake_request();

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style_patch(
            panel,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
    let _ = app.take_wake_request();

    assert!(
        !app.request_node_repaint(window, node).expect("repaint"),
        "祖先不可见 ⇒ 子树不绘制，不产生帧"
    );
    assert!(
        !app.take_wake_request(),
        "不可见子树的请求不得唤醒平台（点不亮的像素不该出帧）"
    );
    assert!(
        !dirty(&app, window, node).contains(DirtyFlags::PAINT),
        "不可见子树不得被标脏"
    );
}

#[test]
fn node_repaint_reports_stale_node_and_unknown_window() {
    let (_rt, mut app, window) = setup();
    let other = app.create_window(WindowOptions {
        title: "other".into(),
        size: Size::new(100.0, 100.0),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    let other_root = app.window(other).expect("window").tree().root();
    let foreign = mount(&mut app, other, other_root, Rect::new(0.0, 0.0, 10.0, 10.0));

    assert!(matches!(
        app.request_node_repaint(window, foreign),
        Err(EngineError::StaleNode { node }) if node == foreign
    ));

    assert!(app.close_window(window));
    assert!(matches!(
        app.request_repaint(window),
        Err(EngineError::UnknownWindow { window: closed }) if closed == window
    ));
    assert!(matches!(
        app.request_node_repaint(window, foreign),
        Err(EngineError::UnknownWindow { window: closed }) if closed == window
    ));
}

#[cfg(feature = "test-support")]
#[test]
fn window_repaint_requests_the_whole_window() {
    let (_rt, mut app, window) = setup();
    let _ = warm(&mut app, window);

    app.request_repaint(window).expect("window repaint");
    assert!(app.take_wake_request(), "窗口级入口同样请求一帧");
    frame(&mut app, window);

    let record = app
        .window(window)
        .expect("window")
        .record()
        .expect("commit 帧必须有记录");
    assert!(record.damage_full, "窗口级入口 = 整窗 damage");
}

#[cfg(feature = "test-support")]
#[test]
fn batched_node_repaints_merge_into_one_wakeup() {
    let (_rt, mut app, window) = setup();
    let Fixture {
        node,
        sibling,
        child_a,
        child_b,
        ..
    } = warm(&mut app, window);
    let requests = app.frame_requests();

    app.batch(|app| {
        assert!(app.request_node_repaint(window, node).expect("repaint"));
        assert!(app.request_node_repaint(window, sibling).expect("repaint"));
        assert!(app.request_node_repaint(window, child_a).expect("repaint"));
    });

    assert_eq!(
        app.frame_requests(),
        requests + 1,
        "批处理内的多次节点重绘只唤醒一次"
    );
    frame(&mut app, window);
    let record = app
        .window(window)
        .expect("window")
        .record()
        .expect("commit 帧必须有记录");
    assert_eq!(
        record.dirty_nodes,
        vec![node, child_a, child_b, sibling],
        "两棵被请求子树的 lane 在同一帧按 DFS 序重建"
    );
}

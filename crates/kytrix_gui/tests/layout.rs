//! L1 集成测试：布局与命中（`TestApp` + `Probe`）。
//!
//! 只走公开 API（AGENTS §5.2）：`NodeBuilder` + `Style`（taffy 树）、`Probe`（布局盒 / 命中）、
//! `App::{make_scroll, scroll_to}`（滚动偏移）。与 `src/layout/**` 的内联单测互补：
//! 这里验证「挂进窗口 → 布局 → 探针/命中」这条**跨模块**链路，不重复 taffy 适配器的单元细节。
//!
//! 需要 `--features test-support`（`Cargo.toml` 的 `[[test]] required-features`）。

use kytrix_gui::{
    interaction::{ScrollAxis, ScrollConfig},
    math::{Point, Rect, Scale, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::{Clip, Style},
    test_support::{app::TestApp, probe::Selector},
    window::WindowOptions,
};
use smol_str::SmolStr;
use taffy::style::{Dimension, FlexDirection, LengthPercentage, LengthPercentageAuto, Position};

const WINDOW: Size = Size::new(400.0, 300.0);

fn app() -> TestApp {
    TestApp::with_options(WindowOptions {
        title: SmolStr::new_static("layout"),
        size: WINDOW,
        scale: Scale::ONE,
    })
}

/// 一个 taffy 样式壳：可选固定尺寸、内边距、主轴方向、gap。
fn style(size: Option<Size>, padding: f32, column: bool, gap: f32) -> Style {
    let edge = LengthPercentage::length(padding);
    let gap = LengthPercentage::length(gap);
    let mut inner = taffy::Style::<SmolStr>::DEFAULT;
    inner.flex_direction = if column {
        FlexDirection::Column
    } else {
        FlexDirection::Row
    };
    inner.flex_shrink = 0.0;
    inner.padding = taffy::geometry::Rect {
        left: edge,
        right: edge,
        top: edge,
        bottom: edge,
    };
    inner.gap = taffy::geometry::Size {
        width: gap,
        height: gap,
    };
    if let Some(size) = size {
        inner.size = taffy::geometry::Size {
            width: Dimension::length(size.width),
            height: Dimension::length(size.height),
        };
    }
    Style {
        inner,
        clip: Clip::None,
    }
}

/// 固定宽度、高度自适应的纵向容器（顶层面板）。
fn panel(width: f32) -> Style {
    let mut style = style(None, 0.0, true, 0.0);
    style.inner.size.width = Dimension::length(width);
    style
}

fn mount(app: &mut TestApp, parent: NodeId, id: &'static str, style: Style) -> NodeId {
    let window = app.window_id();
    NodeBuilder::boxed()
        .style(style)
        .id(id)
        .build(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            parent,
        )
        .expect("挂载")
}

fn node(app: &TestApp, id: &'static str) -> NodeId {
    app.probe()
        .by_selector(Selector::Id(id))
        .unwrap_or_else(|| panic!("节点 `{id}` 必须存在"))
}

fn bounds(app: &TestApp, id: &'static str) -> Rect {
    app.probe().bounds(node(app, id)).expect("布局盒")
}

#[test]
fn flex_row_and_column_place_children_on_the_main_axis() {
    let mut app = app();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();

    // 顶层面板：纵向排列、宽度撑满窗口（根是默认 Row，所以先包一层）。
    let panel = mount(&mut app, root, "panel", panel(WINDOW.width));
    let row = mount(&mut app, panel, "row", style(None, 0.0, false, 8.0));
    mount(
        &mut app,
        row,
        "a",
        style(Some(Size::new(40.0, 20.0)), 0.0, false, 0.0),
    );
    mount(
        &mut app,
        row,
        "b",
        style(Some(Size::new(60.0, 20.0)), 0.0, false, 0.0),
    );
    let col = mount(
        &mut app,
        panel,
        "col",
        style(Some(Size::new(80.0, 100.0)), 0.0, true, 4.0),
    );
    mount(
        &mut app,
        col,
        "c",
        style(Some(Size::new(20.0, 10.0)), 0.0, true, 0.0),
    );
    mount(
        &mut app,
        col,
        "d",
        style(Some(Size::new(20.0, 10.0)), 0.0, true, 0.0),
    );
    let _ = app.drain().expect("布局帧");

    let (a, b) = (bounds(&app, "a"), bounds(&app, "b"));
    assert_eq!(
        (a.origin.x, a.origin.y),
        (bounds(&app, "row").origin.x, 0.0),
        "行首子项与父项左对齐"
    );
    assert_eq!(
        b.origin.x,
        a.origin.x + 40.0 + 8.0,
        "行内兄弟 = 前一项 + gap"
    );
    assert_eq!(b.origin.y, a.origin.y, "同一行 y 相同");
    assert_eq!(
        (a.size.width, a.size.height),
        (40.0, 20.0),
        "固定尺寸不被撑开"
    );

    let (c, d) = (bounds(&app, "c"), bounds(&app, "d"));
    assert_eq!(
        d.origin.y,
        c.origin.y + 10.0 + 4.0,
        "列内兄弟 = 前一项 + gap"
    );
    assert_eq!(d.origin.x, c.origin.x, "同一列 x 相同");
    assert_eq!(
        bounds(&app, "row").origin.y + bounds(&app, "row").size.height,
        bounds(&app, "col").origin.y,
        "面板纵向排布"
    );
}

#[test]
fn fixed_size_and_padding_are_both_honoured() {
    let mut app = app();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();

    let pad = mount(
        &mut app,
        root,
        "pad",
        style(Some(Size::new(120.0, 120.0)), 10.0, true, 0.0),
    );
    mount(
        &mut app,
        pad,
        "padded",
        style(Some(Size::new(30.0, 30.0)), 0.0, true, 0.0),
    );
    let _ = app.drain().expect("布局帧");

    let outer = bounds(&app, "pad");
    let padded = bounds(&app, "padded");
    assert_eq!((outer.size.width, outer.size.height), (120.0, 120.0));
    assert_eq!((padded.size.width, padded.size.height), (30.0, 30.0));
    assert_eq!(padded.origin.x, outer.origin.x + 10.0, "padding-left 生效");
    assert_eq!(padded.origin.y, outer.origin.y + 10.0, "padding-top 生效");
}

#[test]
fn hit_test_finds_the_topmost_node_and_falls_back_to_the_parent() {
    let mut app = app();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();

    let row = mount(&mut app, root, "row", style(None, 0.0, false, 8.0));
    mount(
        &mut app,
        row,
        "hit-a",
        style(Some(Size::new(40.0, 20.0)), 0.0, false, 0.0),
    );
    mount(
        &mut app,
        row,
        "hit-b",
        style(Some(Size::new(60.0, 20.0)), 0.0, false, 0.0),
    );
    let _ = app.drain().expect("布局帧");

    let (a, b) = (bounds(&app, "hit-a"), bounds(&app, "hit-b"));
    let inside_b = Point::new(
        b.origin.x + b.size.width / 2.0,
        b.origin.y + b.size.height / 2.0,
    );
    let hit = app.probe().hit(inside_b).expect("b 区域必须命中");
    assert_eq!(hit.node, node(&app, "hit-b"));

    // 两个子项之间是父节点的空白（gap），只能命中父节点。
    let gap = Point::new(
        a.origin.x + a.size.width + 4.0,
        a.origin.y + a.size.height / 2.0,
    );
    let hit = app.probe().hit(gap).expect("gap 区域必须命中父节点");
    assert_eq!(hit.node, row);

    // 窗口外：无命中。
    assert!(app.probe().hit(Point::new(-1.0, -1.0)).is_none());
}

#[test]
fn scroll_offset_moves_what_hit_test_sees() {
    let mut app = app();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();

    // 滚动容器：200×100，内容溢出到 400 高。
    let mut viewport = style(Some(Size::new(200.0, 100.0)), 0.0, true, 0.0);
    viewport.clip = Clip::Bounds;
    let container = mount(&mut app, root, "viewport", viewport);

    // 子项放在**内容坐标** y = 150（视口外）。
    let mut deep_style = style(Some(Size::new(200.0, 20.0)), 0.0, true, 0.0);
    deep_style.inner.position = Position::Absolute;
    deep_style.inner.inset = taffy::geometry::Rect {
        left: LengthPercentageAuto::length(0.0),
        top: LengthPercentageAuto::length(150.0),
        right: LengthPercentageAuto::auto(),
        bottom: LengthPercentageAuto::auto(),
    };
    mount(&mut app, container, "deep", deep_style);
    let _ = app.drain().expect("布局帧");

    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: 24.0,
            },
        )
        .expect("可滚动");
    app.app_mut()
        .set_scroll_content(&handle, Size::new(200.0, 400.0))
        .expect("内容尺寸");

    let container_box = bounds(&app, "viewport");
    let probe_point = Point::new(container_box.origin.x + 10.0, container_box.origin.y + 40.0);
    let before = app.probe().hit(probe_point).expect("容器区域命中");
    assert_eq!(before.node, container, "滚动前 y=40 是容器的空白");

    app.app_mut()
        .scroll_to(&handle, Point::new(0.0, 120.0))
        .expect("滚动");
    let _ = app.drain().expect("滚动帧");

    let after = app.probe().hit(probe_point).expect("滚动后命中");
    assert_eq!(
        after.node,
        node(&app, "deep"),
        "滚动 120 后内容 y=150 落到 y=30..50，命中子项"
    );
}

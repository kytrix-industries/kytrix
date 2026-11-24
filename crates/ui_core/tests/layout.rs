use smol_str::SmolStr;
use taffy::style::{FlexDirection, Position};
use ui_core::{
    interaction::{ScrollAxis, ScrollConfig},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        style::{Clip, Style},
        style_builder::StyleBuilder,
    },
    test_support::{app::TestApp, probe::Selector},
    window::WindowOptions,
};

const WINDOW: LogicalSize = LogicalSize::new(px(400.0), px(300.0));

fn app() -> TestApp {
    TestApp::with_options(WindowOptions {
        title: SmolStr::new_static("layout"),
        size: WINDOW,
        ..WindowOptions::default()
    })
}

fn style(size: Option<LogicalSize>, padding: f32, column: bool, gap: f32) -> Style {
    let mut style = StyleBuilder::new()
        .flex_none()
        .padding(px(padding))
        .gap(px(gap))
        .build();
    style.flex_direction(if column {
        FlexDirection::Column
    } else {
        FlexDirection::Row
    });
    if let Some(size) = size {
        style.size_length(size.width, size.height);
    }
    style
}

fn panel(width: LogicalPx) -> Style {
    let mut style = style(None, 0.0, true, 0.0);
    style.width_length(width);
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

fn bounds(app: &TestApp, id: &'static str) -> LogicalRect {
    app.probe().bounds(node(app, id)).expect("布局盒")
}

#[test]
fn flex_row_and_column_place_children_on_the_main_axis() {
    let mut app = app();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();

    let panel = mount(&mut app, root, "panel", panel(WINDOW.width));
    let row = mount(&mut app, panel, "row", style(None, 0.0, false, 8.0));
    mount(
        &mut app,
        row,
        "a",
        style(Some(LogicalSize::new(px(40.0), px(20.0))), 0.0, false, 0.0),
    );
    mount(
        &mut app,
        row,
        "b",
        style(Some(LogicalSize::new(px(60.0), px(20.0))), 0.0, false, 0.0),
    );
    let col = mount(
        &mut app,
        panel,
        "col",
        style(Some(LogicalSize::new(px(80.0), px(100.0))), 0.0, true, 4.0),
    );
    mount(
        &mut app,
        col,
        "c",
        style(Some(LogicalSize::new(px(20.0), px(10.0))), 0.0, true, 0.0),
    );
    mount(
        &mut app,
        col,
        "d",
        style(Some(LogicalSize::new(px(20.0), px(10.0))), 0.0, true, 0.0),
    );
    let _ = app.drain().expect("布局帧");

    let (a, b) = (bounds(&app, "a"), bounds(&app, "b"));
    assert_eq!(
        (a.origin.x, a.origin.y),
        (bounds(&app, "row").origin.x, px(0.0)),
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
        (px(40.0), px(20.0)),
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
        style(
            Some(LogicalSize::new(px(120.0), px(120.0))),
            10.0,
            true,
            0.0,
        ),
    );
    mount(
        &mut app,
        pad,
        "padded",
        style(Some(LogicalSize::new(px(30.0), px(30.0))), 0.0, true, 0.0),
    );
    let _ = app.drain().expect("布局帧");

    let outer = bounds(&app, "pad");
    let padded = bounds(&app, "padded");
    assert_eq!(
        (outer.size.width, outer.size.height),
        (px(120.0), px(120.0))
    );
    assert_eq!(
        (padded.size.width, padded.size.height),
        (px(30.0), px(30.0))
    );
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
        style(Some(LogicalSize::new(px(40.0), px(20.0))), 0.0, false, 0.0),
    );
    mount(
        &mut app,
        row,
        "hit-b",
        style(Some(LogicalSize::new(px(60.0), px(20.0))), 0.0, false, 0.0),
    );
    let _ = app.drain().expect("布局帧");

    let (a, b) = (bounds(&app, "hit-a"), bounds(&app, "hit-b"));
    let inside_b = LogicalPoint::new(
        b.origin.x + b.size.width / 2.0,
        b.origin.y + b.size.height / 2.0,
    );
    let hit = app.probe().hit(inside_b).expect("b 区域必须命中");
    assert_eq!(hit.node, node(&app, "hit-b"));

    let gap = LogicalPoint::new(
        a.origin.x + a.size.width + 4.0,
        a.origin.y + a.size.height / 2.0,
    );
    let hit = app.probe().hit(gap).expect("gap 区域必须命中父节点");
    assert_eq!(hit.node, row);

    assert!(
        app.probe()
            .hit(LogicalPoint::new(px(-1.0), px(-1.0)))
            .is_none()
    );
}

#[test]
fn scroll_offset_moves_what_hit_test_sees() {
    let mut app = app();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();

    let mut viewport = style(Some(LogicalSize::new(px(200.0), px(100.0))), 0.0, true, 0.0);
    viewport.clip = Clip::Bounds;
    let container = mount(&mut app, root, "viewport", viewport);

    let mut deep_style = style(Some(LogicalSize::new(px(200.0), px(20.0))), 0.0, true, 0.0);
    deep_style.position(Position::Absolute);
    deep_style.inset_left(px(0.0)).inset_top(px(150.0));
    mount(&mut app, container, "deep", deep_style);
    let _ = app.drain().expect("布局帧");

    let handle = app
        .app_mut()
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(24.0),
            },
        )
        .expect("可滚动");
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("内容尺寸");

    let container_box = bounds(&app, "viewport");
    let probe_point =
        LogicalPoint::new(container_box.origin.x + 10.0, container_box.origin.y + 40.0);
    let before = app.probe().hit(probe_point).expect("容器区域命中");
    assert_eq!(before.node, container, "滚动前 y=40 是容器的空白");

    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(120.0)))
        .expect("滚动");
    let _ = app.drain().expect("滚动帧");

    let after = app.probe().hit(probe_point).expect("滚动后命中");
    assert_eq!(
        after.node,
        node(&app, "deep"),
        "滚动 120 后内容 y=150 落到 y=30..50，命中子项"
    );
}

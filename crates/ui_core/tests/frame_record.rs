use ui_core::{
    math::{Rect, Scale},
    node::builder::NodeBuilder,
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::GeometryId,
        primitive::{Border, Quad, QuadPaint},
        style::{PaintSpec, StylePatch},
    },
    test_support::{
        app::TestApp,
        probe::Selector,
        record::{LaneOutcome, PrimitiveKind},
        script::{FrameScript, ScriptFrame},
    },
    text::TextStyle,
    window::FrameOutcome,
};

const LABEL: Rect = Rect::new(10.0, 10.0, 40.0, 16.0);

#[test]
fn text_frame_records_glyph_run_instances() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口存在").tree().root();
    let _text = NodeBuilder::text("Kytrix", TextStyle::new(12.0))
        .layout_override(LABEL)
        .id("label")
        .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
        .expect("挂载成功");

    let record = app.drain().expect("帧成功").clone();
    assert!(record.rendered, "文本节点必须有像素");
    let rendered = app.rendered().expect("已提交");
    assert!(
        rendered
            .primitives
            .iter()
            .any(|primitive| primitive.kind == PrimitiveKind::GlyphRun),
        "帧记录必须包含 GlyphRun：{:?}",
        rendered.primitives
    );
    assert!(record.stats.instances > 0, "字形必须产生实例");
    assert!(
        record.stats.text.is_some_and(|text| text.shaped > 0),
        "帧记录必须带上文本统计（形状 pass 真的跑过）"
    );
    assert!(record.damage.iter().all(|rect| !rect.is_empty()));
}

#[test]
fn canvas_frame_flattens_primitives() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口存在").tree().root();

    let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 40.0, 20.0));
    canvas.push_quad(Quad {
        bounds: Rect::new(0.0, 0.0, 20.0, 10.0),
        corner_radius: [0.0; 4],
        paint: QuadPaint::Solid {
            fill: Color::WHITE,
            border: None,
        },
    });
    let _canvas = NodeBuilder::canvas(canvas)
        .layout_override(LABEL)
        .id("chart")
        .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
        .expect("挂载成功");

    let record = app.drain().expect("帧成功").clone();
    let rendered = app.rendered().expect("已提交");
    assert!(
        rendered
            .primitives
            .iter()
            .any(|primitive| primitive.kind == PrimitiveKind::Canvas),
        "Canvas 以整块进入 Scene 记录：{:?}",
        rendered.primitives
    );
    assert_eq!(record.stats.instances, 1, "内部 quad 被展平为 1 个实例");
    app.assert_oracle();
}

#[test]
fn geometry_frame_records_geometry_draw() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口存在").tree().root();
    let _geometry = NodeBuilder::geometry(GeometryId::for_test(0, 1))
        .layout_override(LABEL)
        .id("mesh")
        .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
        .expect("挂载成功");

    let record = app.drain().expect("帧成功").clone();
    let rendered = app.rendered().expect("已提交");
    assert!(
        rendered
            .primitives
            .iter()
            .any(|primitive| primitive.kind == PrimitiveKind::Geometry),
        "帧记录必须包含 Geometry：{:?}",
        rendered.primitives
    );
    assert_eq!(
        record.stats.instances, 0,
        "Geometry 走 geometry 通道，0 实例"
    );
}

#[test]
fn damage_is_local_and_idle_is_zero_frame() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口存在").tree().root();
    let background = app.app_mut().signal(Color::WHITE);
    let label = {
        NodeBuilder::boxed()
            .layout_override(LABEL)
            .paint(PaintSpec {
                background: Color::WHITE,
                border: Some(Border {
                    width: 1.0,
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .bind_style(move |cx| StylePatch {
                background: Some(*cx.read(&background)),
                ..StylePatch::default()
            })
            .id("label")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载成功")
    };

    let static_node = NodeBuilder::boxed()
        .layout_override(Rect::new(200.0, 10.0, 40.0, 16.0))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .id("static")
        .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
        .expect("挂载成功");

    let mut script = FrameScript::new(app);
    script.push(ScriptFrame::new("首帧"));
    script.push(ScriptFrame::new("只改 label 的颜色").write(background, Color::BLACK));
    script.push(ScriptFrame::new("空闲帧"));
    script.run().expect("帧脚本");
    let report = script.report();

    report.assert_rendered(0);
    assert_eq!(
        script.app().probe().by_selector(Selector::Id("label")),
        Some(label),
        "稳定 id 必须可查"
    );
    let first_index = report.frame(0).record.frame_index;

    report.assert_rendered(1);
    let frame_area = Rect::new(0.0, 0.0, 800.0, 600.0)
        .to_physical_covering(Scale::new(1.0).expect("scale"))
        .area();
    let damage = &report.frame(1).record.damage;
    assert!(
        damage.iter().all(|rect| rect.area() < frame_area / 50),
        "damage 必须局部：{damage:?}"
    );
    let paint_log = &report.frame(1).record.paint_log;
    assert!(
        !paint_log
            .lanes
            .iter()
            .any(|entry| entry.node == static_node),
        "未变化的兄弟节点不得被访问（脏子树剪枝）：{paint_log:?}"
    );
    assert!(
        paint_log.lanes.iter().any(|entry| entry.node == label),
        "被改动的节点必须被访问并重建 lane：{paint_log:?}"
    );
    assert!(
        paint_log
            .lanes
            .iter()
            .filter(|entry| entry.node != label)
            .all(|entry| entry.outcome == LaneOutcome::Reused),
        "被访问且未重建的 lane 必须整桶复用：{paint_log:?}"
    );
    report.assert_damage_rects_at_most(1, 1);

    report.assert_idle(2);
    assert_eq!(
        report.frame(2).record.frame_index,
        report.frame(1).record.frame_index
    );
    assert_eq!(
        report.frame(1).record.frame_index.get(),
        first_index.get() + 1
    );
    assert!(report.frame(2).record.damage.is_empty());
}

#[test]
fn frame_outcome_is_reported() {
    let mut app = TestApp::new();
    let first = app.window().drain().expect("帧");
    assert!(matches!(first, FrameOutcome::Committed(_)));
    let second = app.window().drain().expect("帧");
    assert_eq!(second, FrameOutcome::Idle);
    assert_eq!(app.frame_record().frame_index.get(), 1);
}

#[test]
fn idle_frames_still_record_the_a11y_snapshot() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口存在").tree().root();
    let button = NodeBuilder::boxed()
        .layout_override(LABEL)
        .on_click(ui_core::event::Action::custom("press"))
        .a11y(ui_core::a11y::A11yOverride {
            label: Some(smol_str::SmolStr::new_static("提交")),
            ..ui_core::a11y::A11yOverride::default()
        })
        .id("submit")
        .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
        .expect("挂载成功");

    let first = app.drain().expect("帧成功").clone();
    let snapshot = first.a11y.as_ref().expect("首帧必须有 a11y 交付");
    assert!(snapshot.full, "首笔必须是全树");
    assert_eq!(
        snapshot.focus_node().engine,
        root,
        "没有焦点时焦点回退到窗口根"
    );

    app.app_mut().set_focus(window, Some(button));
    let record = app.drain().expect("帧成功").clone();
    assert_eq!(record.frame_index, first.frame_index, "空闲帧不提交");
    let snapshot = record.a11y.as_ref().expect("空闲帧也要有 a11y 交付");
    assert_eq!(
        snapshot.focus_node().engine,
        button,
        "焦点节点必须独立携带（不依赖 nodes）"
    );
    assert!(record.damage.is_empty(), "a11y 更新不产生 damage");
    let _ = FrameOutcome::Idle;
}

#[test]
fn incremental_frame_keeps_the_delta_order_fast_path() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口存在").tree().root();
    let color = app.app_mut().signal(Color::WHITE);
    let mut static_nodes = Vec::new();
    for index in 0..64 {
        static_nodes.push(
            NodeBuilder::boxed()
                .layout_override(Rect::new(
                    (index % 8) as f32 * 40.0,
                    (index / 8) as f32 * 20.0 + 40.0,
                    32.0,
                    16.0,
                ))
                .paint(PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                })
                .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
                .expect("静态兄弟"),
        );
    }
    let label = NodeBuilder::boxed()
        .layout_override(LABEL)
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .bind_style(move |cx| StylePatch {
            background: Some(*cx.read(&color)),
            ..StylePatch::default()
        })
        .id("label")
        .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
        .expect("挂载成功");

    let first = app.drain().expect("首帧").clone();
    assert!(first.rendered, "首帧必须提交");
    let node_count = app.app().window(window).expect("窗口").tree().len();
    let counters_before = app
        .app()
        .window(window)
        .expect("窗口")
        .tree()
        .paint_counters();

    app.app_mut().write(&color, Color::BLACK);
    let record = app.drain().expect("增量帧").clone();
    assert!(record.rendered, "颜色变化必须出帧");
    assert!(!record.delta.full, "增量帧不得整帧发布：{:?}", record.delta);
    assert!(
        !record.delta.order_some,
        "没有结构 / lane 成员变化的帧必须沿用旧绘制序（否则编码器退回 O(lanes) 全扫）：{:?}",
        record.delta
    );
    assert_eq!(
        record.delta.lanes, 1,
        "只有脏 lane 变化：{:?}",
        record.delta
    );
    assert_eq!(record.delta.removed, 0);
    assert_eq!(record.delta.scroll, 0);
    assert_eq!(
        record.stats.encoded_lanes, 1,
        "编码器只能重编码那一条脏 lane（其余 {} 段复用）",
        record.stats.reused_segments
    );

    let counters = app
        .app()
        .window(window)
        .expect("窗口")
        .tree()
        .paint_counters();
    assert!(
        counters.visited_nodes <= 4,
        "单节点变更帧的访问数必须只随祖先链增长：{}（树共 {node_count} 节点）",
        counters.visited_nodes
    );
    assert!(
        counters.visited_nodes * 10 < node_count,
        "不得出现全树扫描：{} vs {node_count}",
        counters.visited_nodes
    );
    assert_eq!(
        counters.full_frames, counters_before.full_frames,
        "增量帧不得退化为全量路径"
    );
    assert_eq!(
        counters.order_rebuilds, counters_before.order_rebuilds,
        "没有 lane 成员变化时不得重建绘制序"
    );
    assert!(
        !record
            .paint_log
            .lanes
            .iter()
            .any(|entry| static_nodes.contains(&entry.node)),
        "静态兄弟子树必须被整棵跳过：{:?}",
        record.paint_log
    );
    assert!(
        record
            .paint_log
            .lanes
            .iter()
            .any(|entry| entry.node == label),
        "被改动的节点必须被访问：{:?}",
        record.paint_log
    );
    app.assert_oracle();
}

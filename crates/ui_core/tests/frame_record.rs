//! L2 集成测试：帧记录（`FrameRecord` / `RenderedFrame` / damage / 空闲零帧）。
//!
//! 只通过公开 API 访问引擎（AGENTS §5.2）；需要 `--features test-support`
//! （`Cargo.toml` 的 `[[test]] required-features`）。

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
        record.stats.text.is_some_and(|text| text.tofu == 0),
        "不能出现 tofu（宿主字体缺失时该断言失败，见 V-46）"
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
    // full-refresh oracle：增量 lane 与「清空全部 lane 重建」必须逐项一致。
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
                background: Some(cx.read(&background)),
                ..StylePatch::default()
            })
            .id("label")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载成功")
    };

    // 一个完全不动的节点：用来断言「未变化的 lane 必须复用」。
    let _static = NodeBuilder::boxed()
        .layout_override(Rect::new(200.0, 10.0, 40.0, 16.0))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .id("static")
        .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
        .expect("挂载成功");

    let first = app.drain().expect("首帧").clone();
    assert!(first.rendered);
    assert_eq!(
        app.probe().by_selector(Selector::Id("label")),
        Some(label),
        "稳定 id 必须可查"
    );
    let first_index = first.frame_index;

    // 只改一个 label 的颜色：damage 必须局部。
    app.app_mut().write(&background, Color::BLACK);
    let second = app.drain().expect("第二帧").clone();
    assert!(second.rendered);
    let frame_area = Rect::new(0.0, 0.0, 800.0, 600.0)
        .to_physical_covering(Scale::new(1.0).expect("scale"))
        .area();
    assert!(
        second
            .damage
            .iter()
            .all(|rect| rect.area() < frame_area / 50),
        "damage 必须局部：{:?}",
        second.damage
    );
    assert!(
        second
            .paint_log
            .lanes
            .iter()
            .any(|entry| entry.outcome == LaneOutcome::Reused),
        "未变化的节点必须复用 lane"
    );

    // 空闲零帧：不提交、序号不变。
    let third = app.drain().expect("空闲帧").clone();
    assert!(!third.rendered);
    assert_eq!(third.frame_index, second.frame_index);
    assert_eq!(second.frame_index.get(), first_index.get() + 1);
    assert!(third.damage.is_empty());
}

#[test]
fn frame_outcome_is_reported() {
    let mut app = TestApp::new();
    // 首帧：根从 0 尺寸撑满窗口 ⇒ 有布局移动 ⇒ 提交。
    let first = app.window().drain().expect("帧");
    assert!(matches!(first, FrameOutcome::Committed(_)));
    // 第二帧：无写、无脏 ⇒ 空闲零帧，序号不变。
    let second = app.window().drain().expect("帧");
    assert_eq!(second, FrameOutcome::Idle);
    assert_eq!(app.frame_record().frame_index.get(), 1);
}

/// P11：`FrameRecord.a11y` 在**空闲帧**也必须存在（纯 `A11Y` 变化不提交像素）。
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

    // 只移动焦点 ⇒ 本帧 `Idle`，但快照仍必须挂到记录上。
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

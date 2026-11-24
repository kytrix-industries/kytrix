use std::sync::Arc;

use super::App;
use crate::{
    a11y::A11yRole,
    error::EngineError,
    math::{LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{color::Color, style::Style},
    text::{TextPassReport, TextStyle},
    text_edit::{EditableSpec, TextAffinity},
    window::{WindowId, WindowOptions},
};

fn new_app() -> (tokio::runtime::Runtime, App, WindowId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    let window = app.create_window(WindowOptions::default());
    (rt, app, window)
}

fn spec(style: TextStyle, multiline: bool) -> EditableSpec {
    EditableSpec {
        role: if multiline {
            A11yRole::MultilineTextInput
        } else {
            A11yRole::TextInput
        },
        label: smol_str::SmolStr::new_static(""),
        placeholder: smol_str::SmolStr::new_static(""),
        multiline,
        masked: false,
        max_length: None,
        submit: None,
        text_style: style,
        caret_color: Color::BLACK,
        selection_color: Color::BLACK,
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    }
}

fn node_style(width: Option<f32>) -> Style {
    let mut style = Style::default();
    style.align_self(taffy::style::AlignSelf::START);
    if let Some(width) = width {
        style.width(taffy::style::Dimension::length(width));
    }
    style
}

fn mount(app: &mut App, window: WindowId, spec: EditableSpec, width: Option<f32>) -> NodeId {
    let w = app.window_mut(window).expect("窗口存在");
    let root = w.tree().root();
    NodeBuilder::editable_text(spec)
        .style(node_style(width))
        .build(w.tree_mut(), root)
        .expect("挂载可编辑文本")
}

fn mount_with_style(app: &mut App, window: WindowId, spec: EditableSpec, style: Style) -> NodeId {
    let w = app.window_mut(window).expect("窗口存在");
    let root = w.tree().root();
    NodeBuilder::editable_text(spec)
        .style(style)
        .build(w.tree_mut(), root)
        .expect("挂载可编辑文本")
}

fn masked_spec(style: TextStyle, max_length: Option<usize>) -> EditableSpec {
    EditableSpec {
        masked: true,
        max_length,
        ..spec(style, false)
    }
}

fn max_length_spec(style: TextStyle, max_length: Option<usize>) -> EditableSpec {
    EditableSpec {
        max_length,
        ..spec(style, false)
    }
}

fn display_mirror(app: &App, window: WindowId, node: NodeId) -> smol_str::SmolStr {
    app.window(window)
        .expect("窗口存在")
        .tree()
        .text_content(node)
        .expect("文本镜像")
        .text
        .clone()
}

fn capped_style(max_width: f32) -> Style {
    let mut style = Style::default();
    style.align_self(taffy::style::AlignSelf::START);
    style.max_width(taffy::style::LengthPercentageAuto::length(max_width));
    style
}

fn refresh(app: &mut App, window: WindowId) -> TextPassReport {
    app.refresh_text(window)
        .expect("窗口存在")
        .expect("文本 pass 不报错")
}

fn layout(app: &mut App, window: WindowId) {
    app.window_mut(window)
        .expect("窗口存在")
        .compute_layout()
        .expect("布局不报错");
}

fn text_layout(app: &mut App, window: WindowId) {
    app.layout_text(window)
        .expect("窗口存在")
        .expect("文本重排不报错");
}

fn bounds(app: &App, window: WindowId, node: NodeId) -> LogicalRect {
    app.window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点存在")
        .layout()
        .bounds
}

fn assert_near(actual: LogicalPx, expected: LogicalPx) {
    assert!(
        (actual - expected).abs() <= px(1e-3),
        "expected {expected}, got {actual}"
    );
}

fn assert_not_laid_out(app: &App, window: WindowId, node: NodeId, reason: &str) {
    let error = app.text_edit_metrics(window, node).expect_err(reason);
    let rendered = format!("{error:?}");
    assert!(
        matches!(
            error,
            EngineError::TextEditLayout { node: reported, detail }
                if reported == node && detail == "not laid out yet"
        ),
        "{reason}: {rendered}"
    );
}

#[test]
fn empty_editable_text_measures_a_positive_line_box() {
    let (_rt, mut app, window) = new_app();
    for size in [px(12.0), px(16.0), px(30.0)] {
        let node = mount(&mut app, window, spec(TextStyle::new(size), false), None);
        refresh(&mut app, window);
        layout(&mut app, window);
        text_layout(&mut app, window);

        let metrics = app
            .text_edit_metrics(window, node)
            .expect("空可编辑文本必须有度量");
        assert!(
            metrics.line_height > LogicalPx::ZERO,
            "size {size} 的空行盒必须为正：{metrics:?}"
        );
        assert_eq!(metrics.line_count, 1, "空文本恒为一行");
        assert!(
            metrics
                .first_baseline
                .is_some_and(|baseline| baseline > LogicalPx::ZERO),
            "空 layout 的 strut 行必须带基线：{metrics:?}"
        );
        assert_near(metrics.content_height, metrics.line_height);

        let bounds = bounds(&app, window, node);
        assert_eq!(bounds.size.width, LogicalPx::ZERO, "空文本宽度必须是 0");
        assert_near(bounds.size.height, metrics.line_height);
    }
}

#[test]
fn empty_editable_line_box_honours_an_explicit_line_height_factor() {
    let (_rt, mut app, window) = new_app();
    let style = TextStyle::new(px(10.0)).with_line_height(2.0);
    let node = mount(&mut app, window, spec(style, true), None);
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    let metrics = app.text_edit_metrics(window, node).expect("度量");
    assert_near(metrics.line_height, px(20.0));
    assert_near(bounds(&app, window, node).size.height, px(20.0));
}

#[test]
fn non_empty_editable_text_measures_its_content_width() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    refresh(&mut app, window);
    layout(&mut app, window);

    assert!(
        app.text_edit_set_value(window, node, "Hello")
            .expect("写入成功"),
        "内容变化必须报告 changed"
    );
    let report = refresh(&mut app, window);
    assert_eq!(report.shaped, 1, "内容变化必须重新 shape 一次");
    layout(&mut app, window);
    text_layout(&mut app, window);

    let measured = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_measured_size(node)
        .expect("测量尺寸");
    assert!(
        measured.width > LogicalPx::ZERO,
        "非空文本必须有内容宽度：{measured:?}"
    );
    let bounds = bounds(&app, window, node);
    assert_near(bounds.size.width, measured.width);
    assert_near(bounds.size.height, measured.height);

    let line = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_line(node)
        .expect("行快照");
    assert!(
        line.glyph_runs().any(|run| !run.glyphs.is_empty()),
        "可编辑文本必须走通光栅化出字形"
    );
}

#[test]
fn multiline_editable_text_wraps_at_the_fixed_width() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        Some(120.0),
    );
    app.text_edit_set_value(window, node, "the quick brown fox jumps over the lazy dog")
        .expect("写入成功");
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    let metrics = app.text_edit_metrics(window, node).expect("度量");
    assert!(
        metrics.line_count > 1,
        "定宽 120 的 43 字符必须断成多行：{metrics:?}"
    );
    assert_eq!(bounds(&app, window, node).size.width, px(120.0));
    assert!(
        metrics.content_height > metrics.line_height,
        "内容高必须是完整行高之和（不截断）：{metrics:?}"
    );
    assert_near(
        metrics.content_height,
        metrics.line_height * metrics.line_count as f32,
    );
    assert_near(
        bounds(&app, window, node).size.height,
        metrics.content_height,
    );
}

#[test]
fn set_value_updates_value_and_measure_in_one_frame() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    refresh(&mut app, window);
    layout(&mut app, window);
    assert_eq!(bounds(&app, window, node).size.width, LogicalPx::ZERO);

    assert!(
        app.text_edit_set_value(window, node, "abc")
            .expect("写入成功")
    );
    assert_eq!(app.text_edit_value(window, node).expect("读取成功"), "abc");
    refresh(&mut app, window);
    layout(&mut app, window);
    let width = bounds(&app, window, node).size.width;
    assert!(
        width > LogicalPx::ZERO,
        "同一次 refresh/layout 后测量宽度必须生效：{width}"
    );

    assert!(
        app.text_edit_set_value(window, node, "x\ny\u{8}z")
            .expect("写入成功"),
        "控制字符清洗后内容变化"
    );
    assert_eq!(
        app.text_edit_value(window, node).expect("读取成功"),
        "xyz",
        "单行必须剔除换行与控制字符"
    );

    assert!(
        !app.text_edit_set_value(window, node, "xyz")
            .expect("重复写入成功"),
        "等值写入不报告 changed"
    );

    let multiline = mount(&mut app, window, spec(TextStyle::new(px(16.0)), true), None);
    app.text_edit_set_value(window, multiline, "a\r\nb")
        .expect("写入成功");
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);
    assert_eq!(
        app.text_edit_value(window, multiline).expect("读取成功"),
        "a\nb",
        "多行保留换行并把 CRLF 归一化为 LF"
    );
    let metrics = app.text_edit_metrics(window, multiline).expect("度量");
    assert_eq!(metrics.line_count, 2, "两个硬行必须各占一行");
}

#[test]
fn metrics_queries_reuse_the_break_memo_without_reshaping() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        Some(120.0),
    );
    app.text_edit_set_value(window, node, "lorem ipsum dolor sit amet consectetur")
        .expect("写入成功");
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    let breaks_before = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_slot(node)
        .expect("文本槽")
        .shaped()
        .layout_breaks();
    let first = app.text_edit_metrics(window, node).expect("度量");
    let second = app.text_edit_metrics(window, node).expect("度量");
    assert_eq!(first, second);
    let breaks_after = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_slot(node)
        .expect("文本槽")
        .shaped()
        .layout_breaks();
    assert_eq!(
        breaks_after, breaks_before,
        "几何查询必须复用 ParleySnapshot 的 break memo，不得重新断行"
    );

    let unchanged = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_content(node)
        .cloned()
        .expect("文本载荷");
    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .set_text_content(node, unchanged);
    let report = refresh(&mut app, window);
    assert_eq!(report.shaped, 0, "同内容重新标脏不得重新 shape");
    assert_eq!(report.reused, 1, "必须命中 shape 缓存");
    assert_eq!(report.skipped, 1, "同快照必须直接跳过重排");

    assert!(
        app.text_edit_set_value(window, node, "delta epsilon zeta")
            .expect("写入成功")
    );
    let changed = refresh(&mut app, window);
    assert_eq!(changed.shaped, 1, "内容变化必须重新 shape 一次");
}

#[test]
fn stale_node_id_is_rejected_and_cannot_alias_a_recycled_slot() {
    let (_rt, mut app, window) = new_app();
    let first = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    app.text_edit_set_value(window, first, "first")
        .expect("写入成功");
    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .remove(first)
        .expect("移除成功");
    let second = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    assert_eq!(first.index(), second.index(), "前置：arena 必须复用 index");
    assert_ne!(first, second);

    assert!(matches!(
        app.text_edit_value(window, first),
        Err(EngineError::StaleNode { node }) if node == first
    ));
    assert!(matches!(
        app.text_edit_set_value(window, first, "corrupted"),
        Err(EngineError::StaleNode { node }) if node == first
    ));
    assert!(matches!(
        app.text_edit_metrics(window, first),
        Err(EngineError::StaleNode { node }) if node == first
    ));
    assert!(matches!(
        app.text_edit_snapshot(window, first),
        Err(EngineError::StaleNode { node }) if node == first
    ));
    assert_eq!(
        app.text_edit_value(window, second).expect("新节点可读"),
        "",
        "旧句柄不得改写或读到新节点的值"
    );
}

#[test]
fn cross_window_node_ids_are_rejected() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    let window_a = app.create_window(WindowOptions::default());
    let window_b = app.create_window(WindowOptions::default());
    let node_a = mount(
        &mut app,
        window_a,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    let node_b = mount(
        &mut app,
        window_b,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    assert_eq!(node_a.index(), node_b.index(), "前置：两窗节点 index 相同");

    assert!(matches!(
        app.text_edit_value(window_a, node_b),
        Err(EngineError::StaleNode { node }) if node == node_b
    ));
    assert!(matches!(
        app.text_edit_set_value(window_a, node_b, "leak"),
        Err(EngineError::StaleNode { node }) if node == node_b
    ));
    assert!(matches!(
        app.text_edit_metrics(window_b, node_a),
        Err(EngineError::StaleNode { node }) if node == node_a
    ));
    assert_eq!(
        app.text_edit_value(window_b, node_b).expect("本窗句柄可读"),
        "",
        "异窗写入不得串到本窗节点"
    );
    app.text_edit_set_value(window_a, node_a, "A")
        .expect("本窗写入成功");
    assert_eq!(
        app.text_edit_value(window_b, node_b).expect("本窗句柄可读"),
        "",
        "A 窗写入不得影响 B 窗同 index 节点"
    );
}

#[test]
fn metrics_are_only_available_after_the_full_text_frame() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        Some(120.0),
    );
    assert_not_laid_out(&app, window, node, "挂载后尚未消费文本脏队列");

    refresh(&mut app, window);
    assert_not_laid_out(&app, window, node, "refresh 后尚未布局");

    layout(&mut app, window);
    assert_not_laid_out(&app, window, node, "compute_layout 后尚未 layout_text");

    text_layout(&mut app, window);
    let laid_out = app.text_edit_metrics(window, node).expect("完整帧后可用");
    assert_eq!(laid_out.line_count, 1, "空文本一行");

    app.text_edit_set_value(window, node, "the quick brown fox jumps over the lazy dog")
        .expect("写入成功");
    assert_not_laid_out(&app, window, node, "set_value 尚未被本帧消费");

    refresh(&mut app, window);
    assert_not_laid_out(&app, window, node, "refresh 后尚未布局");
    layout(&mut app, window);
    assert_not_laid_out(&app, window, node, "compute_layout 后尚未 layout_text");
    text_layout(&mut app, window);
    let laid_out = app.text_edit_metrics(window, node).expect("消费后可用");
    assert!(laid_out.line_count > 1, "长文本定宽必须多行");

    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .set_style(node, node_style(Some(60.0)))
        .expect("设置新宽度");
    layout(&mut app, window);
    assert_not_laid_out(&app, window, node, "宽度变化后尚未 layout_text");
    text_layout(&mut app, window);
    let narrowed = app.text_edit_metrics(window, node).expect("重排后可用");
    assert!(
        narrowed.line_count > laid_out.line_count,
        "更窄宽度必须产生更多行：{narrowed:?} vs {laid_out:?}"
    );
}

#[test]
fn metrics_report_the_final_layout_for_an_auto_width_capped_node() {
    let (_rt, mut app, window) = new_app();
    let node = mount_with_style(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        capped_style(120.0),
    );
    app.text_edit_set_value(window, node, "the quick brown fox jumps over the lazy dog")
        .expect("写入成功");

    refresh(&mut app, window);
    assert_not_laid_out(
        &app,
        window,
        node,
        "refresh 后不得返回 max-content 断行数值",
    );
    layout(&mut app, window);
    assert_not_laid_out(
        &app,
        window,
        node,
        "compute_layout 后、layout_text 前不得返回",
    );
    text_layout(&mut app, window);

    let metrics = app.text_edit_metrics(window, node).expect("完整帧后可用");
    let line = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_line(node)
        .expect("行快照")
        .clone();
    assert!(metrics.line_count > 1, "max_width 120 必须断行");
    assert_eq!(
        metrics.line_count,
        line.lines.len(),
        "metrics 必须与真实 raster 行数一致"
    );
    assert_near(metrics.content_height, line.height);
    assert_eq!(bounds(&app, window, node).size.width, px(120.0));
}

#[test]
fn zero_height_editable_node_still_reports_metrics() {
    let (_rt, mut app, window) = new_app();
    let mut style = node_style(None);
    style.height_length(px(0.0));
    let node = mount_with_style(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        style,
    );
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    assert_eq!(bounds(&app, window, node).size.height, LogicalPx::ZERO);
    let metrics = app
        .text_edit_metrics(window, node)
        .expect("零高节点仍可度量");
    assert!(metrics.line_height > LogicalPx::ZERO);
    assert_eq!(metrics.line_count, 1);
}

#[test]
fn empty_editable_caret_geometry_uses_the_line_box() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    let metrics = app.text_edit_metrics(window, node).expect("度量");
    let caret = app
        .window(window)
        .expect("窗口存在")
        .text_caret_rect(node, 0)
        .expect("空文本 caret 几何必须可用");
    assert_eq!(caret.origin.x, LogicalPx::ZERO, "空文本 caret 在行首");
    assert_eq!(caret.origin.y, LogicalPx::ZERO, "空文本 caret 在行盒顶");
    assert!(caret.size.width > LogicalPx::ZERO);
    assert_near(caret.size.height, metrics.line_height);
}

#[test]
fn non_editable_empty_text_still_measures_zero() {
    let (_rt, mut app, window) = new_app();
    let node = {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::text("", TextStyle::new(px(16.0)))
            .style(node_style(None))
            .build(w.tree_mut(), root)
            .expect("挂载文本")
    };
    refresh(&mut app, window);
    layout(&mut app, window);

    assert_eq!(
        bounds(&app, window, node).size,
        LogicalSize::ZERO,
        "非可编辑空文本的测量语义不得改变"
    );
    assert_eq!(
        app.window(window)
            .expect("窗口存在")
            .tree()
            .text_measured_size(node),
        Some(LogicalSize::ZERO)
    );
    assert!(matches!(
        app.text_edit_metrics(window, node),
        Err(EngineError::TextEditNotAvailable { node: reported }) if reported == node
    ));
}

#[test]
fn text_edit_apis_report_strongly_typed_errors() {
    let (_rt, mut app, window) = new_app();
    let text = {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::text("label", TextStyle::new(px(16.0)))
            .build(w.tree_mut(), root)
            .expect("挂载文本")
    };
    assert!(matches!(
        app.text_edit_value(window, text),
        Err(EngineError::TextEditNotAvailable { node }) if node == text
    ));
    assert!(matches!(
        app.text_edit_set_value(window, text, "x"),
        Err(EngineError::TextEditNotAvailable { node }) if node == text
    ));
    assert!(matches!(
        app.text_edit_snapshot(window, text),
        Err(EngineError::TextEditNotAvailable { node }) if node == text
    ));

    let editable = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    assert_eq!(
        app.text_edit_value(window, editable).expect("初始值可读"),
        ""
    );
    assert_eq!(
        app.text_edit_snapshot(window, editable)
            .expect("初始快照可读")
            .generation,
        0
    );
    let unknown = WindowId::from_raw(999);
    assert!(matches!(
        app.text_edit_value(unknown, editable),
        Err(EngineError::UnknownWindow { window }) if window == unknown
    ));
    assert!(matches!(
        app.text_edit_snapshot(unknown, editable),
        Err(EngineError::UnknownWindow { window }) if window == unknown
    ));
    assert!(matches!(
        app.text_edit_metrics(window, editable),
        Err(EngineError::TextEditLayout { node, .. }) if node == editable
    ));

    let mut masked_multiline = spec(TextStyle::new(px(16.0)), true);
    masked_multiline.masked = true;
    let before = app.window(window).expect("窗口存在").tree().len();
    let error = {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::editable_text(masked_multiline)
            .build(w.tree_mut(), root)
            .expect_err("非法 spec 必须挂载失败")
    };
    assert!(matches!(
        error,
        EngineError::TextEditSpecInvalid { detail, .. }
            if detail == "masked editable text cannot be multiline"
    ));
    assert_eq!(
        app.window(window).expect("窗口存在").tree().len(),
        before,
        "失败的挂载不得留下孤儿节点"
    );
}

#[test]
fn masked_display_mirrors_one_bullet_per_grapheme_and_keeps_real_text_apart() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
    );
    for (raw, expected) in [
        ("abc", "•••"),
        ("e\u{301}", "•"),
        ("👨‍👩‍👧", "•"),
        ("🇨🇳", "•"),
        ("👍🏽", "•"),
        ("a👍🏽🇨🇳e\u{301}", "••••"),
    ] {
        assert!(
            app.text_edit_set_value(window, node, raw)
                .expect("写入成功")
        );
        assert_eq!(
            app.text_edit_value(window, node).expect("真实文本"),
            raw,
            "value 必须返回真实文本"
        );
        assert_eq!(
            display_mirror(&app, window, node),
            expected,
            "TextContent 镜像必须是显示文本"
        );
        assert_eq!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .editable(node)
                .expect("可编辑槽位")
                .display_text(),
            expected
        );
        let snapshot = app.text_edit_snapshot(window, node).expect("快照");
        assert_eq!(snapshot.text.as_ref(), raw);
        assert_eq!(snapshot.selection.range(), expected.len()..expected.len());
        assert!(snapshot.selection.is_collapsed());
    }
}

#[test]
fn masked_display_mapping_rebuilds_only_when_the_text_changes() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
    );
    let builds = |app: &App| {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .editable(node)
            .expect("可编辑槽位")
            .display_builds()
    };
    assert_eq!(builds(&app), 0, "挂载只建空 identity display");
    assert!(
        app.text_edit_set_value(window, node, "abc")
            .expect("写入成功")
    );
    assert_eq!(builds(&app), 1, "内容变化重建一次映射");
    for _ in 0..8 {
        let first = app.text_edit_snapshot(window, node).expect("快照");
        let second = app.text_edit_snapshot(window, node).expect("快照");
        assert_eq!(first.generation, second.generation);
        assert!(Arc::ptr_eq(&first.text, &second.text));
    }
    assert_eq!(builds(&app), 1, "读取快照不得重建映射");
    assert!(
        !app.text_edit_set_value(window, node, "abc")
            .expect("等值写入")
    );
    assert_eq!(builds(&app), 1, "等值写入不得重建映射");
    assert!(
        app.text_edit_set_value(window, node, "abcdef")
            .expect("写入成功")
    );
    assert_eq!(builds(&app), 2, "每次内容变化只重建一次");
}

#[test]
fn masked_empty_text_keeps_the_line_box_and_an_empty_display() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
    );
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    assert_eq!(app.text_edit_value(window, node).expect("真实文本"), "");
    assert_eq!(display_mirror(&app, window, node), "");
    let metrics = app
        .text_edit_metrics(window, node)
        .expect("空掩码文本可度量");
    assert_eq!(metrics.line_count, 1);
    assert!(metrics.line_height > LogicalPx::ZERO);
    let snapshot = app.text_edit_snapshot(window, node).expect("快照");
    assert_eq!(snapshot.selection.range(), 0..0);
    assert_eq!(
        snapshot.selection.focus().affinity,
        TextAffinity::Downstream
    );
    assert!(!app.text_edit_set_value(window, node, "").expect("空值写入"));
}

#[test]
fn masked_max_length_counts_the_real_graphemes() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), Some(2)),
        None,
    );
    assert!(
        app.text_edit_set_value(window, node, "👨‍👩‍👧a")
            .expect("写入成功")
    );
    assert_eq!(app.text_edit_value(window, node).expect("真实文本"), "👨‍👩‍👧a");
    assert_eq!(display_mirror(&app, window, node), "••");
    assert!(
        app.text_edit_set_value(window, node, "🇨🇳🇨🇳🇨🇳")
            .expect("写入成功")
    );
    assert_eq!(app.text_edit_value(window, node).expect("真实文本"), "🇨🇳🇨🇳");
    assert_eq!(display_mirror(&app, window, node), "••");
    let snapshot = app.text_edit_snapshot(window, node).expect("快照");
    assert_eq!(
        snapshot.selection.range(),
        6..6,
        "display 坐标是两个 • 的 6 字节"
    );

    let zero = mount(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), Some(0)),
        None,
    );
    assert!(
        !app.text_edit_set_value(window, zero, "abc")
            .expect("max_length=0 静默截断")
    );
    assert_eq!(app.text_edit_value(window, zero).expect("真实文本"), "");
    assert_eq!(display_mirror(&app, window, zero), "");
}

#[test]
fn max_length_truncates_on_real_grapheme_boundaries() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        max_length_spec(TextStyle::new(px(16.0)), Some(2)),
        None,
    );
    assert!(
        app.text_edit_set_value(window, node, "a👍🏽b")
            .expect("写入成功")
    );
    assert_eq!(
        app.text_edit_value(window, node).expect("真实文本"),
        "a👍🏽",
        "截断不得拆开 emoji 字素簇"
    );
    assert_eq!(display_mirror(&app, window, node), "a👍🏽");
    assert!(
        !app.text_edit_set_value(window, node, "a👍🏽")
            .expect("等值写入不报告 changed")
    );

    let unlimited = mount(
        &mut app,
        window,
        max_length_spec(TextStyle::new(px(16.0)), None),
        None,
    );
    assert!(
        app.text_edit_set_value(window, unlimited, "a👍🏽b")
            .expect("写入成功")
    );
    assert_eq!(
        app.text_edit_value(window, unlimited).expect("真实文本"),
        "a👍🏽b"
    );
}

#[test]
fn set_value_collapses_the_selection_to_the_display_end() {
    let (_rt, mut app, window) = new_app();
    let plain = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    let initial = app.text_edit_snapshot(window, plain).expect("初始快照");
    assert_eq!(initial.selection.range(), 0..0);
    assert!(
        app.text_edit_set_value(window, plain, "abc")
            .expect("写入成功")
    );
    let collapsed = app.text_edit_snapshot(window, plain).expect("快照");
    assert!(collapsed.selection.is_collapsed());
    assert_eq!(collapsed.selection.range(), 3..3);
    assert_eq!(collapsed.selection.focus().byte, 3);
    assert_eq!(collapsed.selection.focus().affinity, TextAffinity::Upstream);
    assert!(
        app.text_edit_set_value(window, plain, "")
            .expect("写入成功")
    );
    let empty = app.text_edit_snapshot(window, plain).expect("快照");
    assert_eq!(empty.selection.range(), 0..0);
    assert_eq!(empty.selection.focus().affinity, TextAffinity::Downstream);

    let masked = mount(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
    );
    assert!(
        app.text_edit_set_value(window, masked, "abc")
            .expect("写入成功")
    );
    let snapshot = app.text_edit_snapshot(window, masked).expect("快照");
    assert_eq!(snapshot.selection.range(), 9..9, "••• 的 display 末尾是 9");
    assert_eq!(snapshot.selection.focus().byte, 9);
    let tree = app.window(window).expect("窗口存在").tree();
    let editable = tree.editable(masked).expect("可编辑槽位");
    assert_eq!(editable.display_text(), "•••");
    assert_eq!(editable.display_byte_for_raw(3), 9);
    assert_eq!(editable.raw_byte_for_display(9), 3);
    assert_eq!(editable.raw_byte_for_display(5), 1);
    assert_eq!(editable.display_byte_for_raw(2), 6);
    assert_eq!(editable.display_byte_for_raw(usize::MAX), 9);
    assert_eq!(editable.raw_byte_for_display(usize::MAX), 3);
}

#[test]
fn snapshot_generation_tracks_changes_and_reuses_the_text_allocation() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    let first = app.text_edit_snapshot(window, node).expect("快照");
    let base = first.generation;
    assert!(!app.text_edit_set_value(window, node, "").expect("等值写入"));
    let unchanged = app.text_edit_snapshot(window, node).expect("快照");
    assert_eq!(unchanged.generation, base, "同值写入不得 +1");
    assert!(
        Arc::ptr_eq(&first.text, &unchanged.text),
        "同 generation 的快照必须复用同一次 Arc<str>"
    );

    assert!(
        app.text_edit_set_value(window, node, "abc")
            .expect("写入成功")
    );
    let changed = app.text_edit_snapshot(window, node).expect("快照");
    assert_eq!(changed.generation, base + 1);
    assert!(!Arc::ptr_eq(&first.text, &changed.text));
    let repeated = app.text_edit_snapshot(window, node).expect("快照");
    assert_eq!(repeated.generation, changed.generation);
    assert!(Arc::ptr_eq(&changed.text, &repeated.text));

    let masked = mount(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
    );
    let before = app.text_edit_snapshot(window, masked).expect("快照");
    assert!(
        app.text_edit_set_value(window, masked, "abc")
            .expect("写入成功")
    );
    let after = app.text_edit_snapshot(window, masked).expect("快照");
    assert_eq!(after.generation, before.generation + 1);
    assert!(!Arc::ptr_eq(&before.text, &after.text));
    assert!(
        app.text_edit_set_value(window, masked, "xyz")
            .expect("写入成功")
    );
    let replaced = app.text_edit_snapshot(window, masked).expect("快照");
    assert_eq!(
        replaced.generation,
        after.generation + 1,
        "真实文本变化必须 +1（即使掩码 display 不变）"
    );
    assert!(!Arc::ptr_eq(&after.text, &replaced.text));
    assert_eq!(replaced.text.as_ref(), "xyz");
    assert_eq!(display_mirror(&app, window, masked), "•••");
}

#[cfg(feature = "test-support")]
#[test]
fn masked_display_renders_bullet_glyphs_through_test_app() {
    use crate::test_support::{app::TestApp, record::PrimitiveKind};

    let mut app = TestApp::new();
    let window = app.window_id();
    let (masked, reference) = {
        let w = app.app_mut().window_mut(window).expect("窗口");
        let root = w.tree().root();
        let masked = NodeBuilder::editable_text(masked_spec(TextStyle::new(px(16.0)), None))
            .style(node_style(None))
            .build(w.tree_mut(), root)
            .expect("挂载掩码输入");
        let reference = NodeBuilder::text("•••", TextStyle::new(px(16.0)))
            .style(node_style(None))
            .build(w.tree_mut(), root)
            .expect("挂载参考文本");
        (masked, reference)
    };
    app.app_mut()
        .text_edit_set_value(window, masked, "abc")
        .expect("写入成功");
    assert_eq!(
        app.app()
            .window(window)
            .expect("窗口")
            .tree()
            .text_content(masked)
            .expect("文本镜像")
            .text,
        "•••"
    );
    let _ = app.drain().expect("帧");

    let rendered = app.rendered().expect("已提交");
    let glyph_bounds = |node: NodeId| {
        rendered
            .primitives
            .iter()
            .find(|primitive| primitive.node == node && primitive.kind == PrimitiveKind::GlyphRun)
            .map(|primitive| primitive.bounds)
            .expect("掩码文本必须渲染出字形")
    };
    let masked_bounds = glyph_bounds(masked);
    let reference_bounds = glyph_bounds(reference);
    assert_near(masked_bounds.size.width, reference_bounds.size.width);
    assert!(masked_bounds.size.width > LogicalPx::ZERO);
}

#[cfg(feature = "test-support")]
#[test]
fn editable_text_renders_glyph_primitives_through_test_app() {
    use crate::test_support::{app::TestApp, record::PrimitiveKind};

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = {
        let root = app.app().window(window).expect("窗口").tree().root();
        let w = app.app_mut().window_mut(window).expect("窗口");
        NodeBuilder::editable_text(spec(TextStyle::new(px(16.0)), false))
            .build(w.tree_mut(), root)
            .expect("挂载可编辑文本")
    };
    app.app_mut()
        .text_edit_set_value(window, node, "WWWW")
        .expect("写入成功");
    let _ = app.drain().expect("帧");

    let rendered = app.rendered().expect("已提交");
    assert!(
        rendered
            .primitives
            .iter()
            .any(|primitive| primitive.kind == PrimitiveKind::GlyphRun),
        "可编辑文本必须经 TestApp 渲染出 GlyphRun 图元"
    );
    let bounds = app
        .app()
        .window(window)
        .expect("窗口")
        .tree()
        .get(node)
        .expect("节点")
        .layout()
        .bounds;
    assert!(bounds.size.height > LogicalPx::ZERO, "文本节点必须有行盒");
}

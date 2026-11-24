use std::{sync::Arc, time::Instant};

use super::App;
use crate::{
    a11y::A11yRole,
    error::EngineError,
    event::{Action, Event, InputEvent, KeyCode, KeyState, Modifiers},
    interaction::{ScrollAxis, ScrollConfig},
    math::{LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        color::Color,
        style::{Clip, Style},
        style_builder::StyleBuilder,
    },
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
        readonly: false,
        disabled: false,
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

fn frame(app: &mut App, window: WindowId) {
    app.frame(window).expect("窗口存在").expect("帧成功");
}

fn command_modifiers() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::META
    } else {
        Modifiers::CONTROL
    }
}

fn word_modifiers() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::ALT
    } else {
        Modifiers::CONTROL
    }
}

fn press(app: &mut App, window: WindowId, code: KeyCode, modifiers: Modifiers) -> bool {
    app.dispatch_input_tracked(window, InputEvent::key_down(code, modifiers))
        .1
}

fn press_repeat(app: &mut App, window: WindowId, code: KeyCode, modifiers: Modifiers) -> bool {
    let event = InputEvent::Key {
        code,
        state: KeyState::Down,
        repeat: true,
        modifiers,
        timestamp: Instant::now(),
    };
    app.dispatch_input_tracked(window, event).1
}

fn type_text(app: &mut App, window: WindowId, text: &str) -> bool {
    let event = InputEvent::Text {
        text: smol_str::SmolStr::new(text),
        timestamp: Instant::now(),
    };
    app.dispatch_input_tracked(window, event).1
}

fn snapshot_of(app: &App, window: WindowId, node: NodeId) -> crate::text_edit::TextEditSnapshot {
    app.text_edit_snapshot(window, node).expect("快照")
}

fn caret_of(app: &App, window: WindowId, node: NodeId) -> usize {
    snapshot_of(app, window, node).selection.focus().byte
}

fn text_of(app: &App, window: WindowId, node: NodeId) -> smol_str::SmolStr {
    app.text_edit_value(window, node).expect("真实文本")
}

fn mount_editable(
    app: &mut App,
    window: WindowId,
    editable: EditableSpec,
    width: Option<f32>,
    value: &str,
) -> NodeId {
    let node = mount(app, window, editable, width);
    app.text_edit_set_value(window, node, value)
        .expect("写入成功");
    frame(app, window);
    assert!(
        app.set_focus(window, Some(node)),
        "可编辑节点必须可以被聚焦"
    );
    node
}

#[test]
fn visual_arrow_keys_and_deletion_land_on_grapheme_boundaries() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "ae\u{301}👍🏽",
    );
    let len = "ae\u{301}👍🏽".len();
    assert_eq!(caret_of(&app, window, node), len, "写值后折叠在末尾");

    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 4, "跳过 👍🏽 整簇");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 1, "跳过 e+combining 整簇");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 0);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 0, "越界不移动");
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        Modifiers::empty()
    ));
    assert_eq!(text_of(&app, window, node), "ae\u{301}👍🏽", "句首退格空操作");

    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 1);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 4);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), len);

    crate::text_edit::set_selection(&mut app, window, node, 0, 0).expect("选择");
    assert!(press(&mut app, window, KeyCode::Delete, Modifiers::empty()));
    assert_eq!(text_of(&app, window, node), "e\u{301}👍🏽");
    assert!(press(&mut app, window, KeyCode::Delete, Modifiers::empty()));
    assert_eq!(text_of(&app, window, node), "👍🏽");
    assert!(press(&mut app, window, KeyCode::End, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 8);
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        Modifiers::empty()
    ));
    assert_eq!(text_of(&app, window, node), "");
    frame(&mut app, window);
}

#[test]
fn word_keys_move_and_delete_whole_words() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "hello world",
    );
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        word_modifiers()
    ));
    assert_eq!(caret_of(&app, window, node), 6, "词移动落在上一个词起点");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        word_modifiers()
    ));
    assert_eq!(caret_of(&app, window, node), 0);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        word_modifiers()
    ));
    assert!(caret_of(&app, window, node) > 0);

    crate::text_edit::set_selection(&mut app, window, node, 11, 11).expect("选择");
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        word_modifiers()
    ));
    assert_eq!(text_of(&app, window, node), "hello ");
    assert_eq!(caret_of(&app, window, node), 6);
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        word_modifiers()
    ));
    assert_eq!(text_of(&app, window, node), "");
    assert_eq!(caret_of(&app, window, node), 0);

    app.text_edit_set_value(window, node, "hello world")
        .expect("写入成功");
    frame(&mut app, window);
    crate::text_edit::set_selection(&mut app, window, node, 0, 0).expect("选择");
    assert!(press(&mut app, window, KeyCode::Delete, word_modifiers()));
    assert_eq!(text_of(&app, window, node), "world");
    assert_eq!(caret_of(&app, window, node), 0);
    frame(&mut app, window);
}

#[test]
fn shift_motions_extend_the_selection_and_escape_collapses_to_its_start() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcdef",
    );
    assert!(press(&mut app, window, KeyCode::Home, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 0);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        Modifiers::SHIFT
    ));
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        Modifiers::SHIFT
    ));
    let selected = snapshot_of(&app, window, node);
    assert_eq!(selected.selection.range(), 0..2);
    assert_eq!(selected.selection.anchor().byte, 0);
    assert_eq!(selected.selection.focus().byte, 2);

    assert!(press(&mut app, window, KeyCode::Escape, Modifiers::empty()));
    let collapsed = snapshot_of(&app, window, node);
    assert!(collapsed.selection.is_collapsed());
    assert_eq!(collapsed.selection.range(), 0..0);

    assert!(press(&mut app, window, KeyCode::End, Modifiers::SHIFT));
    let extended = snapshot_of(&app, window, node);
    assert_eq!(extended.selection.range(), 0..6);
    assert!(press(&mut app, window, KeyCode::Escape, Modifiers::empty()));
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        0..0,
        "Escape 折叠到选区起点"
    );
}

#[test]
fn home_end_and_document_motions_follow_visual_and_logical_lines() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "abc\ndef",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("选择");
    assert!(press(&mut app, window, KeyCode::End, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 3, "硬行尾在新行符前");
    assert!(press(&mut app, window, KeyCode::Home, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 0);
    assert!(press(&mut app, window, KeyCode::End, command_modifiers()));
    assert_eq!(caret_of(&app, window, node), 7);
    assert!(press(&mut app, window, KeyCode::Home, command_modifiers()));
    assert_eq!(caret_of(&app, window, node), 0);

    crate::text_edit::set_selection(&mut app, window, node, 5, 5).expect("选择");
    assert!(press(&mut app, window, KeyCode::Home, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 4, "第二行行首");
    assert!(press(&mut app, window, KeyCode::End, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 7);

    crate::text_edit::set_selection(&mut app, window, node, 0, 0).expect("选择");
    assert!(press(
        &mut app,
        window,
        KeyCode::PageDown,
        Modifiers::empty()
    ));
    assert_eq!(
        caret_of(&app, window, node),
        7,
        "无滚动祖先时 PageDown 到文末"
    );
    assert!(press(&mut app, window, KeyCode::PageUp, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 0, "PageUp 回到文首");
}

#[test]
fn up_and_down_remember_the_target_column_across_short_lines() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "longlonglong\nab\nlonglonglong",
    );
    assert!(
        app.text_edit_metrics(window, node)
            .expect("度量")
            .line_count
            > 2
    );
    crate::text_edit::set_selection(&mut app, window, node, 4, 4).expect("选择");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowDown,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 15, "短行夹到行尾");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowDown,
        Modifiers::empty()
    ));
    assert_eq!(
        caret_of(&app, window, node),
        20,
        "列记忆让第二次下行回到第 4 列而不是短行的第 2 列"
    );
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowUp,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 15);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowUp,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 4);

    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowUp,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 0, "首行向上到行首");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowDown,
        Modifiers::empty()
    ));
    assert!(caret_of(&app, window, node) > 0);
}

#[test]
fn soft_wrapped_lines_move_by_visual_line_and_keep_the_break_affinity() {
    let (_rt, mut app, window) = new_app();
    let text = "the quick brown fox jumps over the lazy dog";
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        Some(60.0),
        text,
    );
    assert!(
        app.text_edit_metrics(window, node)
            .expect("度量")
            .line_count
            > 1,
        "60px 宽必须软换行"
    );
    crate::text_edit::set_selection(&mut app, window, node, 0, 0).expect("选择");
    assert!(press(&mut app, window, KeyCode::End, Modifiers::empty()));
    let wrap = snapshot_of(&app, window, node);
    let wrap_byte = wrap.selection.focus().byte;
    assert!(wrap_byte > 0 && wrap_byte < text.len());
    assert_eq!(
        wrap.selection.focus().affinity,
        TextAffinity::Upstream,
        "软换行行尾 affinity = Upstream"
    );
    assert!(press(&mut app, window, KeyCode::Home, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 0, "Home 回到视觉行首");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowDown,
        Modifiers::empty()
    ));
    let second_line = snapshot_of(&app, window, node);
    let second_byte = second_line.selection.focus().byte;
    assert!(second_byte > 0);
    assert!(press(&mut app, window, KeyCode::Home, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), second_byte, "第二视觉行行首");
    assert!(press(&mut app, window, KeyCode::End, Modifiers::empty()));
    assert!(caret_of(&app, window, node) >= second_byte);
}

#[test]
fn consecutive_typing_undoes_in_one_step_and_navigation_breaks_the_group() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "a"));
    assert!(type_text(&mut app, window, "b"));
    assert!(type_text(&mut app, window, "c"));
    assert_eq!(text_of(&app, window, node), "abc");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "", "连续键入合并为一步");
    assert!(!app.text_edit_undo(window, node).expect("undo 耗尽"));
    assert!(app.text_edit_redo(window, node).expect("redo"));
    assert_eq!(text_of(&app, window, node), "abc");

    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        Modifiers::empty()
    ));
    assert!(type_text(&mut app, window, "d"));
    assert_eq!(text_of(&app, window, node), "abcd");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "abc", "导航打断分组");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "");
    assert!(app.text_edit_redo(window, node).expect("redo"));
    assert_eq!(text_of(&app, window, node), "abc");
    frame(&mut app, window);
}

#[test]
fn new_edits_clear_the_redo_stack() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "a"));
    assert!(app.text_edit_undo(window, node).expect("undo"));
    let rewound = snapshot_of(&app, window, node);
    assert!(rewound.can_redo);
    assert!(!rewound.can_undo);
    assert!(type_text(&mut app, window, "b"));
    let edited = snapshot_of(&app, window, node);
    assert!(!edited.can_redo, "新编辑清空 redo");
    assert!(!app.text_edit_redo(window, node).expect("redo 耗尽"));
    assert_eq!(text_of(&app, window, node), "b");
}

#[test]
fn enter_breaks_the_undo_group_in_multiline_text() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "a"));
    assert!(type_text(&mut app, window, "b"));
    assert!(press(&mut app, window, KeyCode::Enter, Modifiers::empty()));
    assert!(type_text(&mut app, window, "c"));
    assert!(type_text(&mut app, window, "d"));
    assert_eq!(text_of(&app, window, node), "ab\ncd");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "ab\n", "撤销后半组键入");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(
        text_of(&app, window, node),
        "ab",
        "换行自身是独立一步（换行打断）"
    );
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "");
    assert!(app.text_edit_redo(window, node).expect("redo"));
    assert_eq!(text_of(&app, window, node), "ab");
    assert!(app.text_edit_redo(window, node).expect("redo"));
    assert_eq!(text_of(&app, window, node), "ab\n");
    assert!(app.text_edit_redo(window, node).expect("redo"));
    assert_eq!(text_of(&app, window, node), "ab\ncd");
    frame(&mut app, window);
}

#[test]
fn undo_list_keeps_the_last_hundred_edits_and_reports_exhaustion() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    for _ in 0..101 {
        assert!(type_text(&mut app, window, "x"));
        assert!(press(
            &mut app,
            window,
            KeyCode::ArrowLeft,
            Modifiers::empty()
        ));
        assert!(press(
            &mut app,
            window,
            KeyCode::ArrowRight,
            Modifiers::empty()
        ));
    }
    assert_eq!(text_of(&app, window, node).len(), 101);
    for _ in 0..100 {
        assert!(app.text_edit_undo(window, node).expect("undo"));
    }
    assert_eq!(
        text_of(&app, window, node).len(),
        1,
        "上限 100 步，最旧一步被丢弃"
    );
    assert!(!app.text_edit_undo(window, node).expect("undo 耗尽"));
    frame(&mut app, window);
}

#[test]
fn undo_history_stores_incremental_ranges_not_whole_documents() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    let typed = 120;
    for _ in 0..typed {
        assert!(type_text(&mut app, window, "x"));
    }
    let stats = app
        .window(window)
        .expect("窗口")
        .tree()
        .editable(node)
        .expect("可编辑槽位")
        .undo_stats();
    assert_eq!(stats, (1, 0, typed), "120 次键入合并为 1 步区间增量");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "");
    frame(&mut app, window);
}

#[test]
fn aria_like_navigation_changes_the_generation_without_touching_history() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abc",
    );
    let before = snapshot_of(&app, window, node);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    let after = snapshot_of(&app, window, node);
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.can_undo, before.can_undo, "导航不改变 undo 历史");
    assert_eq!(after.can_redo, before.can_redo, "导航不改变 redo 历史");
    assert_eq!(after.text.as_ref(), "abc");
}

#[test]
fn enter_submits_a_single_line_input_once_and_ignores_auto_repeat() {
    use std::{cell::Cell, rc::Rc};

    let (_rt, mut app, window) = new_app();
    let submitted = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&submitted);
    app.on_action(Action::custom("submit"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.submit = Some(Action::custom("submit"));
    let node = mount_editable(&mut app, window, editable, None, "abc");

    assert!(press(&mut app, window, KeyCode::Enter, Modifiers::empty()));
    assert_eq!(text_of(&app, window, node), "abc", "单行 Enter 不插入换行");
    assert_eq!(submitted.get(), 1);
    assert!(press_repeat(
        &mut app,
        window,
        KeyCode::Enter,
        Modifiers::empty()
    ));
    assert_eq!(submitted.get(), 1, "按住的 Enter 不重复提交");
    assert!(press(
        &mut app,
        window,
        KeyCode::NumpadEnter,
        Modifiers::empty()
    ));
    assert_eq!(submitted.get(), 2, "NumpadEnter 等价于 Enter");
    frame(&mut app, window);
}

#[test]
fn enter_inserts_newlines_in_multiline_input_even_on_repeat() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "",
    );
    assert!(press(&mut app, window, KeyCode::Enter, Modifiers::empty()));
    assert_eq!(text_of(&app, window, node), "\n");
    assert!(press_repeat(
        &mut app,
        window,
        KeyCode::Enter,
        Modifiers::empty()
    ));
    assert_eq!(text_of(&app, window, node), "\n\n");
    frame(&mut app, window);
}

#[test]
fn text_events_are_sanitized_and_consumed_by_the_focused_input() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "a\u{8}\tb"));
    assert_eq!(text_of(&app, window, node), "ab", "单行过滤控制字符");
    assert!(type_text(&mut app, window, "e\u{301}"));
    assert_eq!(text_of(&app, window, node), "abe\u{301}");
    frame(&mut app, window);

    app.set_focus(window, None);
    assert!(!type_text(&mut app, window, "x"), "无焦点输入框不消费 Text");
}

#[test]
fn masked_editing_maps_display_coordinates_back_to_real_text() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "a"));
    assert!(type_text(&mut app, window, "b"));
    assert!(type_text(&mut app, window, "c"));
    assert_eq!(text_of(&app, window, node), "abc");
    assert_eq!(display_mirror(&app, window, node), "•••");
    assert_eq!(caret_of(&app, window, node), 9);

    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 3);
    assert!(type_text(&mut app, window, "z"));
    assert_eq!(text_of(&app, window, node), "azbc");
    assert_eq!(display_mirror(&app, window, node), "••••");
    assert_eq!(caret_of(&app, window, node), 6);
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        Modifiers::empty()
    ));
    assert_eq!(text_of(&app, window, node), "abc");
    assert_eq!(display_mirror(&app, window, node), "•••");
    assert_eq!(caret_of(&app, window, node), 3);

    assert!(app.set_clipboard_text("xy").is_ok());
    assert!(press(&mut app, window, KeyCode::KeyV, command_modifiers()));
    assert_eq!(
        text_of(&app, window, node),
        "axybc",
        "掩码输入允许粘贴（仅禁止复制/剪切）"
    );
    assert_eq!(display_mirror(&app, window, node), "•••••");

    assert!(app.set_clipboard_text("seed").is_ok());
    assert!(press(&mut app, window, KeyCode::KeyA, command_modifiers()));
    assert!(press(&mut app, window, KeyCode::KeyC, command_modifiers()));
    assert_eq!(
        app.clipboard_text().expect("剪贴板"),
        "seed",
        "掩码文本禁止复制"
    );
    assert!(
        !crate::text_edit::copy_selection(&mut app, window, node).expect("复制命令"),
        "掩码复制命令返回 false"
    );

    assert!(app.set_clipboard_text("seed2").is_ok());
    assert!(press(&mut app, window, KeyCode::KeyA, command_modifiers()));
    let before_cut = text_of(&app, window, node);
    assert!(press(&mut app, window, KeyCode::KeyX, command_modifiers()));
    assert_eq!(text_of(&app, window, node), before_cut, "掩码文本禁止剪切");
    assert_eq!(
        app.clipboard_text().expect("剪贴板"),
        "seed2",
        "掩码剪切不得写入剪贴板"
    );
    frame(&mut app, window);
}

#[test]
fn masked_combining_cluster_edit_keeps_the_caret_at_the_cluster_end() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
        "",
    );
    assert!(
        app.text_edit_set_value(window, node, "\u{301}")
            .expect("写入组合字符")
    );
    assert_eq!(display_mirror(&app, window, node), "•");
    assert_eq!(caret_of(&app, window, node), 3);
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 0);
    assert!(type_text(&mut app, window, "e"));
    assert_eq!(
        text_of(&app, window, node),
        "e\u{301}",
        "插入的 base 字符与后继组合符合并为一个簇"
    );
    assert_eq!(display_mirror(&app, window, node), "•");
    assert_eq!(
        caret_of(&app, window, node),
        3,
        "caret 必须吸附到合并后簇的 display 末尾"
    );
    assert!(type_text(&mut app, window, "x"));
    assert_eq!(text_of(&app, window, node), "e\u{301}x");
    assert_eq!(display_mirror(&app, window, node), "••");
    assert_eq!(caret_of(&app, window, node), 6);
    frame(&mut app, window);
}

#[test]
fn max_length_counts_graphemes_for_typed_text() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        max_length_spec(TextStyle::new(px(16.0)), Some(2)),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "👨‍👩‍👧"));
    assert!(type_text(&mut app, window, "x"));
    assert!(type_text(&mut app, window, "y"));
    assert_eq!(
        text_of(&app, window, node),
        "👨‍👩‍👧x",
        "max_length 按真实字素计数且不拆簇"
    );
    frame(&mut app, window);
}

#[test]
fn empty_text_navigation_and_deletion_are_noops() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    for (code, modifiers) in [
        (KeyCode::ArrowLeft, Modifiers::empty()),
        (KeyCode::ArrowRight, Modifiers::empty()),
        (KeyCode::ArrowUp, Modifiers::empty()),
        (KeyCode::ArrowDown, Modifiers::empty()),
        (KeyCode::Home, Modifiers::empty()),
        (KeyCode::End, Modifiers::empty()),
        (KeyCode::PageUp, Modifiers::empty()),
        (KeyCode::PageDown, Modifiers::empty()),
        (KeyCode::Backspace, Modifiers::empty()),
        (KeyCode::Delete, Modifiers::empty()),
    ] {
        assert!(
            press(&mut app, window, code, modifiers),
            "{code:?} 必须被消费"
        );
        assert_eq!(snapshot_of(&app, window, node).selection.range(), 0..0);
    }
    assert!(type_text(&mut app, window, "a"));
    assert_eq!(text_of(&app, window, node), "a");
    frame(&mut app, window);
}

#[test]
fn set_selection_converges_out_of_range_and_mid_cluster_bytes() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "e\u{301}x",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, node, usize::MAX, 2).expect("选择"));
    let selection = snapshot_of(&app, window, node).selection;
    assert_eq!(
        selection.range(),
        0..4,
        "尾端收敛到 display 末尾，簇内 byte 收敛到簇起点"
    );
    assert!(!selection.is_collapsed());

    assert!(crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("选择"));
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 0..0);
    frame(&mut app, window);
}

#[test]
fn readonly_inputs_allow_navigation_and_copy_but_block_mutations() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.readonly = true;
    let node = mount_editable(&mut app, window, editable, None, "hello");
    assert!(type_text(&mut app, window, "x"));
    assert_eq!(text_of(&app, window, node), "hello", "readonly 拒绝插入");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 4, "readonly 允许导航");
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        Modifiers::empty()
    ));
    assert_eq!(text_of(&app, window, node), "hello");
    assert!(press(&mut app, window, KeyCode::KeyA, command_modifiers()));
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 0..5);
    assert!(press(&mut app, window, KeyCode::KeyC, command_modifiers()));
    assert_eq!(
        app.clipboard_text().expect("剪贴板"),
        "hello",
        "readonly 允许复制"
    );
    assert!(press(&mut app, window, KeyCode::KeyX, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "hello", "readonly 拒绝剪切");
    assert!(app.set_clipboard_text("pasted").is_ok());
    assert!(press(&mut app, window, KeyCode::KeyV, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "hello", "readonly 拒绝粘贴");
    assert!(press(&mut app, window, KeyCode::KeyZ, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "hello", "readonly 拒绝撤销");
    assert!(!app.text_edit_undo(window, node).expect("程序化撤销"));
    frame(&mut app, window);
}

#[test]
fn disabled_editable_text_is_not_focusable_and_leaves_events_to_the_host() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.disabled = true;
    let node = mount(&mut app, window, editable, None);
    app.text_edit_set_value(window, node, "locked")
        .expect("程序化写入");
    frame(&mut app, window);
    assert!(!app.set_focus(window, Some(node)), "disabled 不可聚焦");
    assert_eq!(app.focused(window), None);
    assert!(!type_text(&mut app, window, "x"), "disabled 不消费 Text");
    assert!(!press(
        &mut app,
        window,
        KeyCode::ArrowDown,
        Modifiers::empty()
    ));
    assert_eq!(text_of(&app, window, node), "locked");

    app.dispatch(
        window,
        Event::Input(InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty())),
    );
    assert_eq!(app.drain_events().len(), 1, "非编辑焦点路径必须继续入队");
}

#[test]
fn consumed_edit_events_stay_out_of_the_drain_events_queue() {
    let (_rt, mut app, window) = new_app();
    let _node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    app.dispatch(
        window,
        Event::Input(InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty())),
    );
    assert!(app.drain_events().is_empty(), "被消费的 Key 不入队");
    app.dispatch(
        window,
        Event::Input(InputEvent::Text {
            text: smol_str::SmolStr::new_static("a"),
            timestamp: Instant::now(),
        }),
    );
    assert!(app.drain_events().is_empty(), "被消费的 Text 不入队");
    app.dispatch(
        window,
        Event::Input(InputEvent::key_down(KeyCode::Tab, Modifiers::empty())),
    );
    let queued = app.drain_events();
    assert_eq!(queued.len(), 1, "Tab 交给引擎焦点导航，事件仍入队");
    assert!(matches!(queued[0].1, Event::Input(InputEvent::Key { .. })));
}

#[test]
fn a_consumed_key_up_stays_out_of_the_event_queue() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abc",
    );
    app.dispatch(
        window,
        Event::Input(InputEvent::key_down(KeyCode::ArrowLeft, Modifiers::empty())),
    );
    assert!(app.drain_events().is_empty(), "被消费的 KeyDown 不入队");
    assert_eq!(app.focused(window), Some(node));
    app.dispatch(
        window,
        Event::Input(InputEvent::key_up(KeyCode::ArrowLeft, Modifiers::empty())),
    );
    assert!(
        app.drain_events().is_empty(),
        "同一按键的 KeyUp 也必须随 Down 一起被消费"
    );

    app.dispatch(
        window,
        Event::Input(InputEvent::key_down(KeyCode::F1, Modifiers::empty())),
    );
    app.dispatch(
        window,
        Event::Input(InputEvent::key_up(KeyCode::F1, Modifiers::empty())),
    );
    assert_eq!(
        app.drain_events().len(),
        2,
        "未消费键的 Down/Up 必须正常入队"
    );
    frame(&mut app, window);
}

#[test]
fn default_key_scroll_is_skipped_while_an_input_is_focused() {
    let (_rt, mut app, window) = new_app();
    let mut style = StyleBuilder::new().size(px(100.0), px(100.0)).build();
    style.clip = Clip::Bounds;
    let (container, node, button) = {
        let tree = app.window_mut(window).expect("窗口").tree_mut();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("滚动容器");
        let node = NodeBuilder::editable_text(spec(TextStyle::new(px(16.0)), false))
            .build(tree, container)
            .expect("输入节点");
        let button = NodeBuilder::boxed()
            .on_click(Action::custom("row"))
            .build(tree, container)
            .expect("按钮");
        (container, node, button)
    };
    app.text_edit_set_value(window, node, "hello")
        .expect("写入成功");
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
        .expect("可滚动");
    app.set_scroll_content(&handle, LogicalSize::new(px(100.0), px(400.0)))
        .expect("内容尺寸");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(node)));
    let before = app.scroll_state(&handle).expect("滚动状态").offset.y;

    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowDown,
        Modifiers::empty()
    ));
    assert_eq!(
        app.scroll_state(&handle).expect("滚动状态").offset.y,
        before,
        "输入框消费方向键后不得滚页面"
    );
    assert!(press(
        &mut app,
        window,
        KeyCode::PageDown,
        Modifiers::empty()
    ));
    assert_eq!(
        app.scroll_state(&handle).expect("滚动状态").offset.y,
        before,
        "PageDown 也不得滚页面"
    );

    assert!(app.set_focus(window, Some(button)));
    assert!(!press(
        &mut app,
        window,
        KeyCode::ArrowDown,
        Modifiers::empty()
    ));
    assert!(
        app.scroll_state(&handle).expect("滚动状态").offset.y > before,
        "非编辑焦点仍走 default_key_scroll"
    );
    frame(&mut app, window);
}

#[test]
fn copy_cut_and_paste_go_through_the_app_clipboard() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "hello ",
    );
    assert!(app.set_clipboard_text("world").is_ok());
    assert!(press(&mut app, window, KeyCode::KeyV, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "hello world");
    assert!(press(&mut app, window, KeyCode::KeyA, command_modifiers()));
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 0..11);
    assert!(press(&mut app, window, KeyCode::KeyX, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "");
    assert_eq!(app.clipboard_text().expect("剪贴板"), "hello world");
    assert!(press(&mut app, window, KeyCode::KeyV, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "hello world");
    assert!(app.text_edit_undo(window, node).expect("撤销粘贴"));
    assert_eq!(text_of(&app, window, node), "", "粘贴前选区已被剪切为空");
    assert!(app.text_edit_undo(window, node).expect("撤销剪切"));
    assert_eq!(text_of(&app, window, node), "hello world");
    assert!(app.text_edit_undo(window, node).expect("撤销首次粘贴"));
    assert_eq!(text_of(&app, window, node), "hello ");
    frame(&mut app, window);
}

#[test]
fn shift_word_and_page_extensions_and_paste_undo_restore_the_selection() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "hello world",
    );
    crate::text_edit::set_selection(&mut app, window, node, 0, 0).expect("选择");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowRight,
        word_modifiers() | Modifiers::SHIFT
    ));
    let word_extended = snapshot_of(&app, window, node).selection;
    assert_eq!(word_extended.range().start, 0);
    assert!(
        word_extended.range().end > 0,
        "Ctrl/Alt+Shift+→ 必须按词扩展选区"
    );
    assert!(press(&mut app, window, KeyCode::PageDown, Modifiers::SHIFT));
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        0..11,
        "Shift+PageDown 在单行文档扩到文末"
    );

    assert!(app.set_clipboard_text("XY").is_ok());
    crate::text_edit::set_selection(&mut app, window, node, 1, 4).expect("选择");
    assert!(press(&mut app, window, KeyCode::KeyV, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "hXYo world");
    assert!(app.text_edit_undo(window, node).expect("撤销粘贴"));
    assert_eq!(text_of(&app, window, node), "hello world");
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        1..4,
        "撤销粘贴必须恢复编辑前选区"
    );
    frame(&mut app, window);
}

#[test]
fn readonly_single_line_enter_still_dispatches_submit_once() {
    use std::{cell::Cell, rc::Rc};

    let (_rt, mut app, window) = new_app();
    let submitted = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&submitted);
    app.on_action(Action::custom("submit"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.readonly = true;
    editable.submit = Some(Action::custom("submit"));
    let node = mount_editable(&mut app, window, editable, None, "hello");
    assert!(press(&mut app, window, KeyCode::Enter, Modifiers::empty()));
    assert_eq!(submitted.get(), 1, "readonly 不改文本但不阻止提交");
    assert_eq!(text_of(&app, window, node), "hello");
    assert!(press_repeat(
        &mut app,
        window,
        KeyCode::Enter,
        Modifiers::empty()
    ));
    assert_eq!(submitted.get(), 1, "auto-repeat 不重复提交");
    frame(&mut app, window);
}

#[test]
fn paste_is_sanitized_and_clipped_by_grapheme_max_length() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        max_length_spec(TextStyle::new(px(16.0)), Some(3)),
        None,
        "",
    );
    assert!(app.set_clipboard_text("a\r\n\tb\u{8}🇨🇳🇨🇳").is_ok());
    assert!(press(&mut app, window, KeyCode::KeyV, command_modifiers()));
    assert_eq!(
        text_of(&app, window, node),
        "ab🇨🇳",
        "粘贴清洗控制字符并按真实字素截断"
    );
    frame(&mut app, window);
}

#[test]
fn redo_shortcuts_accept_both_control_y_and_shift_z() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "a"));
    assert!(press(&mut app, window, KeyCode::KeyZ, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "");
    assert!(press(
        &mut app,
        window,
        KeyCode::KeyZ,
        command_modifiers() | Modifiers::SHIFT
    ));
    assert_eq!(text_of(&app, window, node), "a", "Shift+Z 重做");
    assert!(press(&mut app, window, KeyCode::KeyZ, command_modifiers()));
    assert!(press(&mut app, window, KeyCode::KeyY, command_modifiers()));
    assert_eq!(text_of(&app, window, node), "a", "Ctrl/Cmd+Y 重做");
    frame(&mut app, window);
}

#[test]
fn consecutive_deletes_merge_into_one_undo_step() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcdef",
    );
    crate::text_edit::set_selection(&mut app, window, node, 0, 0).expect("选择");
    for _ in 0..3 {
        assert!(press(&mut app, window, KeyCode::Delete, Modifiers::empty()));
    }
    assert_eq!(text_of(&app, window, node), "def");
    let stats = app
        .window(window)
        .expect("窗口")
        .tree()
        .editable(node)
        .expect("可编辑槽位")
        .undo_stats();
    assert_eq!(stats, (1, 0, 3), "连续向后删除合并为一步区间增量");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "abcdef");

    crate::text_edit::set_selection(&mut app, window, node, 6, 6).expect("选择");
    for _ in 0..3 {
        assert!(press(
            &mut app,
            window,
            KeyCode::Backspace,
            Modifiers::empty()
        ));
    }
    assert_eq!(text_of(&app, window, node), "abc");
    let stats = app
        .window(window)
        .expect("窗口")
        .tree()
        .editable(node)
        .expect("可编辑槽位")
        .undo_stats();
    assert_eq!(stats, (1, 0, 3), "连续退格合并为一步区间增量");
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "abcdef");
    frame(&mut app, window);
}

#[test]
fn page_keys_use_the_scroll_viewport_height_for_line_counts() {
    let (_rt, mut app, window) = new_app();
    let mut style = StyleBuilder::new().size(px(100.0), px(100.0)).build();
    style.clip = Clip::Bounds;
    let (container, node) = {
        let tree = app.window_mut(window).expect("窗口").tree_mut();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("滚动容器");
        let node = NodeBuilder::editable_text(spec(TextStyle::new(px(16.0)), true))
            .build(tree, container)
            .expect("多行输入");
        (container, node)
    };
    let text = "a\nb\nc\nd\ne";
    app.text_edit_set_value(window, node, text)
        .expect("写入成功");
    frame(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(20.0),
            },
        )
        .expect("可滚动");
    app.set_scroll_content(&handle, LogicalSize::new(px(100.0), px(300.0)))
        .expect("内容尺寸");
    frame(&mut app, window);
    assert!(app.set_focus(window, Some(node)));
    crate::text_edit::set_selection(&mut app, window, node, 0, 0).expect("选择");
    assert!(press(
        &mut app,
        window,
        KeyCode::PageDown,
        Modifiers::empty()
    ));
    let metrics = app.text_edit_metrics(window, node).expect("度量");
    let page_lines = (100.0 / metrics.line_height.value()).floor() as usize;
    let last_line = metrics.line_count.saturating_sub(1);
    let expected = if page_lines > last_line {
        text.len()
    } else {
        page_lines * 2
    };
    assert_eq!(
        caret_of(&app, window, node),
        expected,
        "视口 100px 按行高换算出的行数与硬行起点一致"
    );
    assert!(press(&mut app, window, KeyCode::PageUp, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 0);
    assert_eq!(
        app.scroll_state(&handle).expect("滚动状态").offset.y,
        LogicalPx::ZERO,
        "翻页键不滚页面"
    );
    frame(&mut app, window);
}

#[test]
fn set_value_clears_history_and_moves_the_caret_to_the_end_even_when_unchanged() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "ab"));
    assert!(press(&mut app, window, KeyCode::Home, Modifiers::empty()));
    assert_eq!(caret_of(&app, window, node), 0);
    assert!(
        !app.text_edit_set_value(window, node, "ab")
            .expect("等值写入"),
        "等值写入不报告内容变化"
    );
    let snapshot = snapshot_of(&app, window, node);
    assert_eq!(snapshot.selection.range(), 2..2, "程序化写入把光标移到末尾");
    assert!(!snapshot.can_undo, "程序化写入清空 undo");
    assert!(!snapshot.can_redo);
    frame(&mut app, window);
}

#[test]
fn typing_over_a_selection_replaces_it_in_one_undo_step() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "hello",
    );
    assert!(press(&mut app, window, KeyCode::KeyA, command_modifiers()));
    assert!(type_text(&mut app, window, "x"));
    assert_eq!(text_of(&app, window, node), "x");
    assert_eq!(caret_of(&app, window, node), 1);
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "hello");
    assert!(!app.text_edit_undo(window, node).expect("undo 耗尽"));
    frame(&mut app, window);
}

#[test]
fn digit_only_editable_text_still_edits_through_the_parley_layout() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "123",
    );
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 2);
    assert!(type_text(&mut app, window, "4"));
    assert_eq!(text_of(&app, window, node), "1243");
    assert_eq!(caret_of(&app, window, node), 3);
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        Modifiers::empty()
    ));
    assert_eq!(text_of(&app, window, node), "123");
    frame(&mut app, window);
}

#[test]
fn editable_nodes_never_reuse_a_cached_simple_snapshot_of_equal_text() {
    use std::rc::Rc;

    let (_rt, mut app, window) = new_app();
    let plain = {
        let w = app.window_mut(window).expect("窗口");
        let root = w.tree().root();
        NodeBuilder::text("123", TextStyle::new(px(16.0)))
            .style(node_style(None))
            .build(w.tree_mut(), root)
            .expect("普通数字文本")
    };
    frame(&mut app, window);
    let plain_shaped = {
        let tree = app.window(window).expect("窗口").tree();
        Rc::clone(tree.text_slot(plain).expect("普通文本槽").shaped())
    };
    assert!(
        plain_shaped.is_simple(),
        "前置：同内容普通数字文本必须命中 fast path 的 Simple 快照"
    );

    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    app.text_edit_set_value(window, node, "123")
        .expect("写入成功");
    #[cfg(feature = "test-support")]
    let shaped_before = app.shape_count_for_test();
    frame(&mut app, window);
    #[cfg(feature = "test-support")]
    assert_eq!(
        app.shape_count_for_test(),
        shaped_before + 1,
        "同内容 editable 不得命中 Simple 缓存，必须新增一次 parley shape"
    );
    assert!(app.set_focus(window, Some(node)));
    let editable_shaped = {
        let tree = app.window(window).expect("窗口").tree();
        Rc::clone(tree.text_slot(node).expect("可编辑文本槽").shaped())
    };
    assert!(
        !Rc::ptr_eq(&plain_shaped, &editable_shaped),
        "editable 必须拿到独立的 parley 快照"
    );
    assert!(
        !editable_shaped.is_simple(),
        "editable 快照必须是 parley Layout"
    );
    let caret = app
        .window(window)
        .expect("窗口")
        .text_caret_rect(node, 1)
        .expect("parley caret 几何必须可用");
    assert!(caret.size.height > LogicalPx::ZERO);

    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(caret_of(&app, window, node), 2, "parley 编辑必须可移动光标");
    assert!(type_text(&mut app, window, "4"));
    assert_eq!(text_of(&app, window, node), "1243");
    frame(&mut app, window);
}

#[test]
fn initial_snapshot_pins_the_engine_contract() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    let snapshot = snapshot_of(&app, window, node);
    assert_eq!(snapshot.generation, 0);
    assert!(!snapshot.composing);
    assert!(!snapshot.focused);
    assert!(!snapshot.can_undo);
    assert!(!snapshot.can_redo);
    assert_eq!(snapshot.text.as_ref(), "");
    assert_eq!(snapshot.selection.range(), 0..0);
    assert_eq!(
        snapshot.selection.focus().affinity,
        TextAffinity::Downstream
    );
}

#[test]
fn mapped_edit_keys_are_consumed_without_losing_focus() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "hello",
    );
    assert!(app.set_clipboard_text("").is_ok());
    for (code, modifiers) in [
        (KeyCode::ArrowLeft, Modifiers::empty()),
        (KeyCode::ArrowRight, Modifiers::empty()),
        (KeyCode::ArrowUp, Modifiers::empty()),
        (KeyCode::ArrowDown, Modifiers::empty()),
        (KeyCode::PageUp, Modifiers::empty()),
        (KeyCode::PageDown, Modifiers::empty()),
        (KeyCode::Home, Modifiers::empty()),
        (KeyCode::End, Modifiers::empty()),
        (KeyCode::Home, command_modifiers()),
        (KeyCode::End, command_modifiers()),
        (KeyCode::Backspace, Modifiers::empty()),
        (KeyCode::Delete, Modifiers::empty()),
        (KeyCode::Escape, Modifiers::empty()),
        (KeyCode::KeyA, command_modifiers()),
        (KeyCode::KeyC, command_modifiers()),
        (KeyCode::KeyX, command_modifiers()),
        (KeyCode::KeyV, command_modifiers()),
        (KeyCode::KeyZ, command_modifiers()),
        (KeyCode::KeyY, command_modifiers()),
        (KeyCode::KeyZ, command_modifiers() | Modifiers::SHIFT),
        (KeyCode::ArrowLeft, word_modifiers()),
        (KeyCode::ArrowRight, word_modifiers()),
        (KeyCode::Backspace, word_modifiers()),
        (KeyCode::Delete, word_modifiers()),
        (KeyCode::KeyA, Modifiers::empty()),
        (KeyCode::Space, Modifiers::empty()),
        (KeyCode::KeyA, Modifiers::SHIFT),
    ] {
        assert_eq!(app.focused(window), Some(node), "断言前焦点必须在输入框");
        assert!(
            press(&mut app, window, code, modifiers),
            "{code:?} + {modifiers:?} 必须被编辑内核消费"
        );
        assert_eq!(
            app.focused(window),
            Some(node),
            "{code:?} + {modifiers:?} 不得改变焦点"
        );
    }
    frame(&mut app, window);
}

#[test]
fn unmapped_keys_are_left_to_the_host_with_focus_intact() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "hello",
    );
    for (code, modifiers) in [
        (KeyCode::KeyS, command_modifiers()),
        (KeyCode::F1, Modifiers::empty()),
        (KeyCode::Escape, Modifiers::SHIFT),
    ] {
        assert_eq!(app.focused(window), Some(node), "断言前焦点必须在输入框");
        assert!(
            !press(&mut app, window, code, modifiers),
            "{code:?} + {modifiers:?} 必须留给宿主"
        );
        assert_eq!(
            app.focused(window),
            Some(node),
            "{code:?} + {modifiers:?} 不得改变焦点"
        );
    }
    frame(&mut app, window);
}

#[test]
fn tab_is_left_to_engine_focus_navigation() {
    let (_rt, mut app, window) = new_app();
    let _node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "hello",
    );
    assert!(!press(&mut app, window, KeyCode::Tab, Modifiers::empty()));
    assert_eq!(
        app.focused(window),
        None,
        "editable 目前不在 Tab 序（P1c 接入），Tab 交给引擎焦点导航"
    );
}

#[test]
fn alt_gr_never_triggers_edit_commands_and_still_lets_text_arrive() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "hello world",
    );
    crate::text_edit::set_selection(&mut app, window, node, 11, 11).expect("选择");
    let alt_gr = Modifiers::CONTROL | Modifiers::ALT;
    let before = text_of(&app, window, node);

    for (code, label) in [
        (KeyCode::Backspace, "AltGr+Backspace 不得按词删除"),
        (KeyCode::Delete, "AltGr+Delete 不得按词删除"),
    ] {
        assert_eq!(app.focused(window), Some(node));
        assert!(
            !press(&mut app, window, code, alt_gr),
            "{label}，应交给宿主"
        );
        assert_eq!(app.focused(window), Some(node));
        assert_eq!(text_of(&app, window, node), before, "{label}");
    }
    assert!(
        !press(&mut app, window, KeyCode::ArrowLeft, alt_gr),
        "AltGr+← 不得触发 Ctrl 词移动/行首命令"
    );
    assert_eq!(text_of(&app, window, node), before);
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 11..11);

    assert!(
        press(&mut app, window, KeyCode::KeyZ, alt_gr),
        "AltGr+可打印键由编辑内核消费，宿主不得触发快捷键"
    );
    assert_eq!(text_of(&app, window, node), before, "AltGr+Z 不得触发撤销");
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 11..11);
    assert!(type_text(&mut app, window, "é"));
    assert_eq!(
        text_of(&app, window, node),
        "hello worldé",
        "AltGr 组合字符仍经 Text 事件插入"
    );
    frame(&mut app, window);
}

#[test]
fn plain_key_events_are_consumed_so_host_shortcuts_do_not_fire_while_typing() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(press(&mut app, window, KeyCode::KeyA, Modifiers::empty()));
    assert!(press(&mut app, window, KeyCode::Space, Modifiers::empty()));
    assert!(press(&mut app, window, KeyCode::Digit1, Modifiers::SHIFT));
    assert_eq!(text_of(&app, window, node), "", "文本只来自 Text 事件");
    frame(&mut app, window);
}

#[test]
fn selection_commands_are_idempotent_for_collapsed_carets() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abc",
    );
    assert!(crate::text_edit::select_all(&mut app, window, node).expect("全选"));
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 0..3);
    assert!(!crate::text_edit::select_all(&mut app, window, node).expect("重复全选"));
    assert!(crate::text_edit::collapse_selection(&mut app, window, node).expect("折叠"));
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 0..0);
    assert!(!crate::text_edit::collapse_selection(&mut app, window, node).expect("重复折叠"));
    frame(&mut app, window);
}

#[cfg(feature = "test-support")]
#[test]
fn each_keystroke_shapes_at_most_once() {
    let (_rt, mut app, window) = new_app();
    let _node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    let base = app.shape_count_for_test();
    assert!(type_text(&mut app, window, "a"));
    assert_eq!(app.shape_count_for_test(), base, "插入本身不 shape");
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(
        app.shape_count_for_test(),
        base + 1,
        "内容变化后的首次布局访问 refresh 一次"
    );
    assert!(press(
        &mut app,
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert_eq!(app.shape_count_for_test(), base + 1, "复用布局不重复 shape");
    frame(&mut app, window);
    assert_eq!(app.shape_count_for_test(), base + 1, "帧管线命中已有 shape");
    assert!(type_text(&mut app, window, "b"));
    assert_eq!(app.shape_count_for_test(), base + 1);
    frame(&mut app, window);
    assert_eq!(app.shape_count_for_test(), base + 2, "每个键最多一次 shape");
    frame(&mut app, window);
    assert_eq!(app.shape_count_for_test(), base + 2, "空闲帧不 shape");
}

use std::{sync::Arc, time::Instant};

use super::App;
use crate::{
    a11y::{A11yAction, A11yNodeId, A11yRole, A11yTextSelection},
    error::EngineError,
    event::{Action, Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers},
    interaction::{ScrollAxis, ScrollConfig},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
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
        role: A11yRole::PasswordInput,
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
    assert!(
        !press(&mut app, window, KeyCode::Escape, Modifiers::empty()),
        "选区已折叠时 Escape 必须留给宿主（对话框关闭等绑定）"
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

fn dispatch_ime(app: &mut App, window: WindowId, event: ImeEvent) -> bool {
    app.dispatch(window, Event::Ime(event));
    !app.drain_events()
        .iter()
        .any(|(_, queued)| matches!(queued, Event::Ime(_)))
}

fn preedit(
    app: &mut App,
    window: WindowId,
    text: &str,
    caret: Option<u32>,
    clauses: smallvec::SmallVec<[std::ops::Range<usize>; 4]>,
    replace: Option<std::ops::Range<usize>>,
) -> bool {
    dispatch_ime(
        app,
        window,
        ImeEvent::Preedit {
            text: smol_str::SmolStr::new(text),
            caret,
            clauses,
            replace,
        },
    )
}

fn commit_ime(
    app: &mut App,
    window: WindowId,
    text: &str,
    replace: Option<std::ops::Range<usize>>,
) -> bool {
    dispatch_ime(
        app,
        window,
        ImeEvent::Commit {
            text: smol_str::SmolStr::new(text),
            replace,
        },
    )
}

fn composition_of(
    app: &App,
    window: WindowId,
    node: NodeId,
) -> Option<crate::text_edit::TextEditComposition> {
    app.text_edit_composition(window, node).expect("组合快照")
}

fn display_builds(app: &App, window: WindowId, node: NodeId) -> u32 {
    app.window(window)
        .expect("窗口存在")
        .tree()
        .editable(node)
        .expect("可编辑槽位")
        .display_builds()
}

#[test]
fn ime_preedit_composes_identity_display_around_the_caret() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(2),
        smallvec::smallvec![0..2],
        None,
    ));
    assert_eq!(display_mirror(&app, window, node), "abnicd");
    assert_eq!(text_of(&app, window, node), "abcd", "组合不写 committed");
    let snapshot = snapshot_of(&app, window, node);
    assert!(snapshot.composing);
    assert!(snapshot.focused);
    assert_eq!(
        snapshot.selection.range(),
        4..4,
        "选区折叠到组合内 caret（ab + ni）"
    );
    let composition = composition_of(&app, window, node).expect("组合期必须有 display 信息");
    assert_eq!(composition.preedit, 2..4);
    assert_eq!(composition.caret, 4);
    assert_eq!(composition.clauses, vec![2..4]);
}

#[test]
fn ime_preedit_masks_each_preedit_grapheme_and_maps_committed_text() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
        "ab",
    );
    assert_eq!(caret_of(&app, window, node), 6, "•• 末尾");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert_eq!(display_mirror(&app, window, node), "••••");
    assert_eq!(text_of(&app, window, node), "ab");
    let snapshot = snapshot_of(&app, window, node);
    assert_eq!(
        snapshot.selection.range(),
        12..12,
        "组合 caret 在两字 preedit 末尾"
    );
    let composition = composition_of(&app, window, node).expect("组合 display");
    assert_eq!(composition.preedit, 6..12);
    assert_eq!(composition.caret, 12);

    assert!(preedit(
        &mut app,
        window,
        "👨‍👩‍👧",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert_eq!(
        display_mirror(&app, window, node),
        "•••",
        "组合串按字素一个 •"
    );
    assert_eq!(
        composition_of(&app, window, node).expect("组合").preedit,
        6..9
    );
    frame(&mut app, window);
    assert_eq!(
        display_mirror(&app, window, node),
        "•••",
        "组合期掩码 display 必须能被文本 pass 整形"
    );
}

#[test]
fn ime_clauses_are_converted_to_display_bytes() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "ab",
    );
    assert!(preedit(
        &mut app,
        window,
        "ni你好",
        Some(2),
        smallvec::smallvec![2..3],
        None,
    ));
    assert_eq!(display_mirror(&app, window, node), "abni你好");
    let composition = composition_of(&app, window, node).expect("组合");
    assert_eq!(composition.preedit, 2..10);
    assert_eq!(composition.caret, 4);
    assert_eq!(
        composition.clauses,
        vec![4..7],
        "UTF-16 2..3 = '你' 的 3 个 display 字节"
    );

    let masked = mount_editable(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
        "ab",
    );
    assert!(preedit(
        &mut app,
        window,
        "ni你好",
        Some(2),
        smallvec::smallvec![2..3],
        None,
    ));
    assert_eq!(display_mirror(&app, window, masked), "••••••");
    let composition = composition_of(&app, window, masked).expect("掩码组合");
    assert_eq!(composition.preedit, 6..18);
    assert_eq!(
        composition.caret, 12,
        "前缀两个 • + 组合内两个 • 的 display caret"
    );
    assert_eq!(
        composition.clauses,
        vec![12..15],
        "前缀两个 • + preedit 两个 • 之后的第三个 • 是 '你' 的高亮 clause"
    );
}

#[test]
fn ime_preedit_replace_splices_into_the_current_preedit() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(2),
        smallvec::SmallVec::new(),
        None
    ));
    assert!(preedit(
        &mut app,
        window,
        "hao",
        Some(3),
        smallvec::SmallVec::new(),
        Some(0..1),
    ));
    assert_eq!(display_mirror(&app, window, node), "abhaoicd");
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        5..5,
        "caret = 替换段前 0 + 'hao' 的 3 字节"
    );
    let composition = composition_of(&app, window, node).expect("组合");
    assert_eq!(composition.preedit, 2..6);
    assert_eq!(composition.caret, 5);
}

#[test]
fn ime_end_cancels_the_preedit_and_restores_the_committed_view() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 1, 3).expect("选区");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert_eq!(display_mirror(&app, window, node), "anid");
    assert_eq!(text_of(&app, window, node), "abcd");
    let before_end = snapshot_of(&app, window, node).generation;
    assert!(dispatch_ime(&mut app, window, ImeEvent::End));
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing);
    assert!(snapshot.generation > before_end);
    assert_eq!(snapshot.selection.range(), 1..1, "caret 回组合起点");
    assert_eq!(display_mirror(&app, window, node), "abcd");
    assert_eq!(text_of(&app, window, node), "abcd");
    assert!(composition_of(&app, window, node).is_none());
    let after_end = snapshot.generation;
    assert!(
        dispatch_ime(&mut app, window, ImeEvent::End),
        "End 必须消费"
    );
    assert_eq!(
        snapshot_of(&app, window, node).generation,
        after_end,
        "重复 End 幂等"
    );
}

#[test]
fn ime_commit_replaces_the_composition_and_undoes_in_one_step() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 1, 3).expect("选区");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(2),
        smallvec::SmallVec::new(),
        None
    ));
    assert!(commit_ime(&mut app, window, "你", None));
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing);
    assert_eq!(
        text_of(&app, window, node),
        "a你d",
        "组合区间被提交文本替换"
    );
    assert_eq!(display_mirror(&app, window, node), "a你d");
    assert_eq!(
        snapshot.selection.range(),
        4..4,
        "display caret 在提交文本之后"
    );
    assert!(snapshot.can_undo);
    assert!(app.text_edit_undo(window, node).expect("撤销"));
    assert_eq!(text_of(&app, window, node), "abcd", "一次提交 = 一步 undo");
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        1..3,
        "撤销恢复组合前选区"
    );
    assert!(!app.text_edit_undo(window, node).expect("没有更多步骤"));
    frame(&mut app, window);
}

#[test]
fn ime_commit_with_replace_commits_the_spliced_preedit() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "haoi",
        Some(2),
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(commit_ime(&mut app, window, "你", Some(1..3)));
    assert_eq!(
        text_of(&app, window, node),
        "abh你icd",
        "replace 相对 marked text 的子区间在提交前 splice"
    );
    assert!(!snapshot_of(&app, window, node).composing);
    frame(&mut app, window);
}

#[test]
fn ime_commit_without_a_composition_inserts_text_directly() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "ab",
    );
    assert!(commit_ime(&mut app, window, "你", None));
    assert_eq!(text_of(&app, window, node), "ab你");
    assert_eq!(display_mirror(&app, window, node), "ab你");
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing);
    assert_eq!(snapshot.selection.range(), 5..5);

    assert!(commit_ime(&mut app, window, "x\u{8}y", None));
    assert_eq!(
        text_of(&app, window, node),
        "ab你xy",
        "无组合的 IME 提交同样清洗控制字符"
    );
}

#[test]
fn ime_commit_clips_to_grapheme_max_length() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        max_length_spec(TextStyle::new(px(16.0)), Some(2)),
        None,
        "a",
    );
    const FAMILY: &str = "👨‍👩‍👧";
    assert!(preedit(
        &mut app,
        window,
        FAMILY,
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(commit_ime(&mut app, window, "🇨🇳x", None));
    assert_eq!(
        text_of(&app, window, node),
        "a🇨🇳",
        "max_length 按真实字素裁剪且不拆 emoji 簇"
    );
    frame(&mut app, window);
}

#[test]
fn ime_text_events_are_suppressed_during_composition() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "ab",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert!(
        type_text(&mut app, window, "x"),
        "组合期普通 Text 必须被编辑内核消费而不是插入"
    );
    assert_eq!(text_of(&app, window, node), "ab");
    assert_eq!(display_mirror(&app, window, node), "abni");
    assert_eq!(
        composition_of(&app, window, node)
            .expect("组合仍存在")
            .preedit,
        2..4
    );
}

#[test]
fn ime_events_are_consumed_only_by_a_focused_editable() {
    let (_rt, mut app, window) = new_app();
    let _node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(
        dispatch_ime(&mut app, window, ImeEvent::Start),
        "焦点在可编辑节点时必须消费 IME"
    );
    assert_eq!(app.drain_events(), vec![], "被消费的 IME 不入事件队列");
    app.set_focus(window, None);
    assert!(
        !dispatch_ime(&mut app, window, ImeEvent::Start),
        "无焦点时 IME 交还宿主事件队列"
    );
}

#[test]
fn disabled_editable_is_not_focusable_and_leaves_ime_to_the_host() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.disabled = true;
    let node = mount(&mut app, window, editable, None);
    app.text_edit_set_value(window, node, "locked")
        .expect("程序化写入");
    frame(&mut app, window);
    assert!(!app.set_focus(window, Some(node)), "disabled 不可聚焦");
    assert_eq!(app.focused(window), None, "disabled 节点无法进入编辑路径");
    assert!(!dispatch_ime(
        &mut app,
        window,
        ImeEvent::Commit {
            text: smol_str::SmolStr::new_static("x"),
            replace: None,
        }
    ));
    assert_eq!(text_of(&app, window, node), "locked");
}

#[test]
fn readonly_editable_text_consumes_ime_without_composing() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.readonly = true;
    let node = mount_editable(&mut app, window, editable, None, "hello");
    assert!(dispatch_ime(&mut app, window, ImeEvent::Start));
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert!(!snapshot_of(&app, window, node).composing);
    assert_eq!(text_of(&app, window, node), "hello");
    assert_eq!(display_mirror(&app, window, node), "hello");
}

#[test]
fn losing_focus_cancels_the_composition() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert!(snapshot_of(&app, window, node).focused);
    assert!(app.set_focus(window, None));
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "失焦必须取消组合");
    assert!(!snapshot.focused);
    assert_eq!(display_mirror(&app, window, node), "abcd");
    assert_eq!(snapshot.selection.range(), 2..2, "caret 回组合起点");

    assert!(app.set_focus(window, Some(node)));
    assert!(snapshot_of(&app, window, node).focused);
}

#[test]
fn switching_focus_between_editables_cancels_the_first_composition() {
    let (_rt, mut app, window) = new_app();
    let first = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    let second = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    app.text_edit_set_value(window, second, "wxyz")
        .expect("写入");
    frame(&mut app, window);
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(app.set_focus(window, Some(second)));
    let first_snapshot = snapshot_of(&app, window, first);
    assert!(!first_snapshot.composing);
    assert!(!first_snapshot.focused);
    assert_eq!(display_mirror(&app, window, first), "abcd");
    assert!(snapshot_of(&app, window, second).focused);
}

#[test]
fn window_focus_loss_cancels_the_composition() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    app.dispatch(window, Event::FocusChanged { focused: false });
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing);
    assert_eq!(display_mirror(&app, window, node), "abcd");
    assert_eq!(snapshot.selection.range(), 4..4, "组合起点在文本末尾");
}

#[test]
fn enter_during_composition_neither_submits_nor_inserts_a_newline() {
    use std::{cell::Cell, rc::Rc};

    let (_rt, mut app, window) = new_app();
    let submitted = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&submitted);
    app.on_action(Action::custom("submit"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.submit = Some(Action::custom("submit"));
    let node = mount_editable(&mut app, window, editable, None, "ab");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert!(
        press(&mut app, window, KeyCode::Enter, Modifiers::empty()),
        "组合期 Enter 必须被消费"
    );
    assert_eq!(submitted.get(), 0, "组合期 Enter 不得 submit");
    assert_eq!(text_of(&app, window, node), "ab");
    assert!(press_repeat(
        &mut app,
        window,
        KeyCode::NumpadEnter,
        Modifiers::empty()
    ));
    assert_eq!(submitted.get(), 0, "组合期 auto-repeat 也不提交");
    assert!(snapshot_of(&app, window, node).composing);
}

#[test]
fn escape_cancels_the_composition_before_collapsing_a_selection() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcdef",
    );
    crate::text_edit::set_selection(&mut app, window, node, 1, 3).expect("选区");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert!(dispatch_ime(&mut app, window, ImeEvent::Start));
    assert!(
        press(&mut app, window, KeyCode::Escape, Modifiers::empty()),
        "组合期 Escape 被编辑内核消费"
    );
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "Escape 优先取消组合");
    assert_eq!(snapshot.selection.range(), 1..1, "取消后折叠到组合起点");

    crate::text_edit::set_selection(&mut app, window, node, 0, 4).expect("选区");
    assert!(press(&mut app, window, KeyCode::Escape, Modifiers::empty()));
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        0..0,
        "无组合时 Escape 折叠选区"
    );
}

#[test]
fn composition_updates_rebuild_the_display_once_per_event_and_never_per_frame() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcdef",
    );
    let base = display_builds(&app, window, node);
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert_eq!(display_builds(&app, window, node), base + 1);
    assert!(preedit(
        &mut app,
        window,
        "nih",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert_eq!(display_builds(&app, window, node), base + 2);
    assert!(preedit(
        &mut app,
        window,
        "nih",
        None,
        smallvec::SmallVec::new(),
        None
    ));
    assert_eq!(
        display_builds(&app, window, node),
        base + 2,
        "相同 preedit 不重建 display"
    );
    let generation = snapshot_of(&app, window, node).generation;
    frame(&mut app, window);
    assert_eq!(
        display_builds(&app, window, node),
        base + 2,
        "帧不重建 display"
    );
    assert_eq!(snapshot_of(&app, window, node).generation, generation);
}

fn a11y_snapshot(app: &mut App, window: WindowId) -> crate::a11y::A11yTreeSnapshot {
    frame(app, window);
    app.a11y_take(window)
        .or_else(|| app.a11y_take(window))
        .expect("a11y 交付")
}

fn a11y_node(app: &mut App, window: WindowId, node: NodeId) -> crate::a11y::A11yNode {
    a11y_snapshot(app, window)
        .node_for(node)
        .cloned()
        .expect("可编辑节点必须进入 a11y 树")
}

#[test]
fn editable_a11y_patch_exposes_role_value_placeholder_and_flags() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.label = smol_str::SmolStr::new_static("搜索");
    editable.placeholder = smol_str::SmolStr::new_static("输入关键词");
    editable.readonly = true;
    let node = mount_editable(&mut app, window, editable, None, "hello");
    let snapshot = a11y_snapshot(&mut app, window);
    let record = snapshot.node_for(node).expect("输入节点").clone();
    assert_eq!(record.role, A11yRole::TextInput);
    assert_eq!(record.label_text(), Some("搜索"));
    assert_eq!(record.value.as_deref(), Some("hello"));
    assert!(record.read_only);
    assert_eq!(record.placeholder.as_deref(), Some("输入关键词"));
    assert_eq!(
        record.text_selection,
        Some(A11yTextSelection {
            anchor: 5,
            focus: 5
        }),
        "选区挂在输入节点上（TextPosition.node 指向 TextRun 子节点）"
    );
    assert!(
        record.character_lengths.is_empty(),
        "字符语义由 TextRun 子节点承担，输入节点不写 character_lengths"
    );
    let run_id = record
        .text_run
        .expect("可编辑节点必须有虚拟 TextRun 子节点");
    assert_eq!(
        record.children.first(),
        Some(&run_id),
        "输入节点的 children 首位必须是它的 TextRun 子节点"
    );
    let run = snapshot.node(run_id).expect("TextRun 子节点");
    assert_eq!(run.role, A11yRole::TextRun);
    assert_eq!(run.value.as_deref(), Some("hello"));
    assert_eq!(run.character_lengths, vec![1, 1, 1, 1, 1]);
    assert_eq!(run.parent, Some(record.id));
    assert_eq!(run.engine, node, "虚拟子节点回指同一个引擎节点");
    assert!(run.actions.contains(&A11yAction::SetTextSelection));
    let ak = crate::a11y::adapter::to_accesskit(&record);
    assert_eq!(ak.role(), accesskit::Role::TextInput);
    assert_eq!(ak.value(), Some("hello"));
    let selection = ak.text_selection().expect("text_selection");
    assert_eq!(selection.focus.character_index, 5);
    assert_eq!(
        selection.focus.node,
        accesskit::NodeId(run_id.get()),
        "TextPosition.node 必须是 TextRun 子节点"
    );
    let ak_run = crate::a11y::adapter::to_accesskit(run);
    assert_eq!(ak_run.role(), accesskit::Role::TextRun);
    assert_eq!(ak_run.character_lengths(), &[1, 1, 1, 1, 1]);
    assert_eq!(ak_run.value(), Some("hello"));
}

#[test]
fn password_a11y_patch_masks_the_value_and_never_leaks_plaintext() {
    let (_rt, mut app, window) = new_app();
    let mut editable = masked_spec(TextStyle::new(px(16.0)), None);
    editable.label = smol_str::SmolStr::new_static("密码");
    let node = mount_editable(&mut app, window, editable, None, "s3cret");
    let snapshot = a11y_snapshot(&mut app, window);
    let record = snapshot.node_for(node).expect("密码输入").clone();
    assert_eq!(record.role, A11yRole::PasswordInput);
    assert_eq!(record.value.as_deref(), Some("••••••"));
    assert!(
        record
            .value
            .as_deref()
            .is_some_and(|value| !value.contains("s3cret"))
    );
    assert_eq!(
        record.text_selection,
        Some(A11yTextSelection {
            anchor: 6,
            focus: 6
        })
    );
    let run_id = record.text_run.expect("密码输入的 TextRun 子节点");
    let run = snapshot.node(run_id).expect("TextRun 子节点");
    assert_eq!(run.value.as_deref(), Some("••••••"), "TextRun 只给掩码串");
    assert!(
        run.value
            .as_deref()
            .is_some_and(|value| !value.contains("s3cret")),
        "TextRun 不得泄漏明文"
    );
    assert_eq!(run.character_lengths, vec![3, 3, 3, 3, 3, 3]);
    let ak = crate::a11y::adapter::to_accesskit(&record);
    assert_eq!(ak.role(), accesskit::Role::PasswordInput);
    assert_eq!(ak.value(), Some("••••••"));
    let selection = ak.text_selection().expect("text_selection");
    assert_eq!(selection.focus.character_index, 6);
    assert_eq!(selection.focus.node, accesskit::NodeId(run_id.get()));
    let ak_run = crate::a11y::adapter::to_accesskit(run);
    assert_eq!(ak_run.value(), Some("••••••"));
    assert_eq!(ak_run.character_lengths(), &[3, 3, 3, 3, 3, 3]);
}

#[test]
fn empty_editable_mounts_with_engine_a11y_patch_before_focus() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.label = smol_str::SmolStr::new_static("搜索");
    let node = mount(&mut app, window, editable, None);
    assert_eq!(app.focused(window), None, "前置：未聚焦");
    let record = a11y_node(&mut app, window, node);
    assert_eq!(
        record.role,
        A11yRole::TextInput,
        "挂载写 patch 必须发生在 a11y_slots 初始化之后"
    );
    assert_eq!(record.label_text(), Some("搜索"));
    assert_eq!(record.value.as_deref(), Some(""), "空文本也交付 value");
    assert_eq!(
        record.text_selection,
        Some(A11yTextSelection {
            anchor: 0,
            focus: 0
        }),
        "空文本的 caret 也必须交付（consumer 空输入同样解析选区）"
    );
    let run_id = record.text_run.expect("空文本也必须有 TextRun 子节点");
    assert!(record.children.contains(&run_id));
}

#[test]
fn disabled_empty_editable_keeps_its_a11y_patch() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.disabled = true;
    editable.label = smol_str::SmolStr::new_static("锁定输入");
    editable.placeholder = smol_str::SmolStr::new_static("不可编辑");
    let node = mount(&mut app, window, editable, None);
    assert!(!app.set_focus(window, Some(node)), "disabled 不可聚焦");
    assert_eq!(app.focused(window), None);
    let record = a11y_node(&mut app, window, node);
    assert_eq!(record.role, A11yRole::TextInput);
    assert_eq!(record.label_text(), Some("锁定输入"));
    assert_eq!(record.value.as_deref(), Some(""));
    assert_eq!(record.placeholder.as_deref(), Some("不可编辑"));
    assert!(record.disabled, "disabled 空输入必须交付 disabled 状态");
}

#[test]
fn composing_preedit_is_not_exposed_to_a11y() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(2),
        smallvec::SmallVec::new(),
        None,
    ));
    let record = a11y_node(&mut app, window, node);
    assert_eq!(
        record.value.as_deref(),
        Some("abcd"),
        "a11y value 不含 preedit"
    );
    assert_eq!(record.text_selection, None, "组合期也不暴露 preedit/选区");
}

#[test]
fn focus_changes_update_the_snapshot_flags_and_generation() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
    );
    let unfocused = snapshot_of(&app, window, node);
    assert!(!unfocused.focused);
    assert_eq!(unfocused.generation, 0);
    assert!(app.set_focus(window, Some(node)));
    let focused = snapshot_of(&app, window, node);
    assert!(focused.focused);
    assert_eq!(focused.generation, unfocused.generation + 1);
    assert!(app.set_focus(window, None));
    let blurred = snapshot_of(&app, window, node);
    assert!(!blurred.focused);
    assert_eq!(blurred.generation, focused.generation + 1);
    assert!(!app.set_focus(window, None), "重复失焦不改变快照");
    assert_eq!(
        snapshot_of(&app, window, node).generation,
        blurred.generation
    );
}

#[test]
fn programmatic_set_value_cancels_the_composition() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(
        app.text_edit_set_value(window, node, "xyz")
            .expect("程序化写入"),
        "写入必须报告内容变化"
    );
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "程序化写入必须取消组合");
    assert_eq!(text_of(&app, window, node), "xyz");
    assert_eq!(display_mirror(&app, window, node), "xyz");
    assert_eq!(snapshot.selection.range(), 3..3);
}

#[test]
fn multiline_composition_commit_keeps_newlines() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "ab",
    );
    assert!(preedit(
        &mut app,
        window,
        "c\nd",
        Some(3),
        smallvec::SmallVec::new(),
        None,
    ));
    assert_eq!(display_mirror(&app, window, node), "abc\nd");
    assert!(commit_ime(&mut app, window, "c\nd", None));
    assert_eq!(text_of(&app, window, node), "abc\nd");
    frame(&mut app, window);
}

#[test]
fn a11y_patch_tracks_edited_values() {
    let (_rt, mut app, window) = new_app();
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.label = smol_str::SmolStr::new_static("搜索");
    let node = mount_editable(&mut app, window, editable, None, "ab");
    assert!(type_text(&mut app, window, "c"));
    let record = a11y_node(&mut app, window, node);
    assert_eq!(record.value.as_deref(), Some("abc"));
    assert_eq!(
        record.text_selection,
        Some(A11yTextSelection {
            anchor: 3,
            focus: 3
        }),
        "输入后 caret 跟在文本末尾"
    );
    assert!(press(
        &mut app,
        window,
        KeyCode::Backspace,
        Modifiers::empty()
    ));
    let record = a11y_node(&mut app, window, node);
    assert_eq!(record.value.as_deref(), Some("ab"));
    assert_eq!(
        record.text_selection,
        Some(A11yTextSelection {
            anchor: 2,
            focus: 2
        })
    );
}

#[test]
fn set_selection_during_composition_uses_committed_coordinates() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(2),
        smallvec::SmallVec::new(),
        None,
    ));
    assert_eq!(display_mirror(&app, window, node), "abnicd");
    assert!(crate::text_edit::set_selection(&mut app, window, node, 0, 3).expect("组合期选区"));
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "设置选区必须先取消组合");
    assert_eq!(
        snapshot.selection.range(),
        0..2,
        "组合 display 0..3 的选区取消组合后落在 committed 的 0..2"
    );
    assert_eq!(display_mirror(&app, window, node), "abcd");
    assert_eq!(text_of(&app, window, node), "abcd");
}

#[test]
fn set_selection_during_masked_composition_maps_to_real_text() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
        "abc",
    );
    assert!(crate::text_edit::set_selection(&mut app, window, node, 3, 6).expect("选中 b"));
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert_eq!(display_mirror(&app, window, node), "••••");
    let _ = crate::text_edit::set_selection(&mut app, window, node, 3, 6).expect("组合期选区");
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing);
    assert_eq!(
        snapshot.selection.range(),
        3..3,
        "组合替换区间内的 display 3..6 取消后映射到组合起点 raw1（display 3..3）；旧顺序会得到 3..6"
    );
    assert_eq!(display_mirror(&app, window, node), "•••");
    assert_eq!(text_of(&app, window, node), "abc");
}

#[test]
fn undo_and_redo_cancel_the_composition() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "",
    );
    assert!(type_text(&mut app, window, "a"));
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(app.text_edit_undo(window, node).expect("撤销"));
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "undo 必须先取消组合");
    assert_eq!(text_of(&app, window, node), "");
    assert_eq!(display_mirror(&app, window, node), "");

    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(app.text_edit_redo(window, node).expect("重做"));
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "redo 必须先取消组合");
    assert_eq!(text_of(&app, window, node), "a");
    assert_eq!(display_mirror(&app, window, node), "a");
}

#[test]
fn ime_preedit_clamps_out_of_range_replace_and_caret() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(9999),
        smallvec::SmallVec::new(),
        None,
    ));
    let composition = composition_of(&app, window, node).expect("组合");
    assert_eq!(composition.preedit, 2..4);
    assert_eq!(composition.caret, 4, "越界 caret 收敛到 preedit 末尾");
    assert!(preedit(
        &mut app,
        window,
        "hao",
        Some(1),
        smallvec::SmallVec::new(),
        Some(99..100),
    ));
    assert_eq!(
        display_mirror(&app, window, node),
        "abnihaocd",
        "越界 replace 收敛到 preedit 末尾后做 suffix splice"
    );
    let composition = composition_of(&app, window, node).expect("组合");
    assert_eq!(composition.preedit, 2..7);
    assert_eq!(composition.caret, 5, "caret 相对 fragment 起点");
    let (reverse_start, reverse_end) = (5usize, 2usize);
    assert!(preedit(
        &mut app,
        window,
        "x",
        Some(0),
        smallvec::SmallVec::new(),
        Some(reverse_start..reverse_end),
    ));
    assert_eq!(
        display_mirror(&app, window, node),
        "abnixcd",
        "反向 replace 区间按规范化后的 2..5 处理"
    );
    let composition = composition_of(&app, window, node).expect("组合");
    assert_eq!(composition.preedit, 2..5);
    assert_eq!(composition.caret, 4);
    frame(&mut app, window);
}

#[test]
fn empty_preedit_keeps_the_composition_until_end() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(dispatch_ime(&mut app, window, ImeEvent::Start));
    assert!(preedit(
        &mut app,
        window,
        "",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    let snapshot = snapshot_of(&app, window, node);
    assert!(snapshot.composing, "空 preedit 仍是组合态");
    assert_eq!(display_mirror(&app, window, node), "abcd");
    let composition = composition_of(&app, window, node).expect("组合信息");
    assert_eq!(composition.preedit, 2..2, "空 preedit 的 display 区间为空");
    assert_eq!(composition.caret, 2);
    assert!(dispatch_ime(&mut app, window, ImeEvent::End));
    assert!(!snapshot_of(&app, window, node).composing);
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 2..2);
}

#[test]
fn masked_composition_commit_is_truncated_and_undoes_cleanly() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), Some(3)),
        None,
        "a",
    );
    assert!(preedit(
        &mut app,
        window,
        "🇨🇳",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert_eq!(display_mirror(&app, window, node), "••");
    assert!(commit_ime(&mut app, window, "🇨🇳xyz", None));
    assert_eq!(
        text_of(&app, window, node),
        "a🇨🇳x",
        "masked 组合提交按真实字素截断到 max_length"
    );
    assert_eq!(display_mirror(&app, window, node), "•••");
    assert!(app.text_edit_undo(window, node).expect("撤销组合提交"));
    assert_eq!(text_of(&app, window, node), "a");
    assert_eq!(display_mirror(&app, window, node), "•");
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 3..3);
}

#[test]
fn no_composition_ime_commit_is_truncated_and_undoes_in_one_step() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        max_length_spec(TextStyle::new(px(16.0)), Some(3)),
        None,
        "a",
    );
    assert!(commit_ime(&mut app, window, "🇨🇳xy", None));
    assert_eq!(
        text_of(&app, window, node),
        "a🇨🇳x",
        "无组合提交同样按真实字素 max_length 截断"
    );
    assert!(app.text_edit_undo(window, node).expect("撤销"));
    assert_eq!(text_of(&app, window, node), "a");
    assert!(!app.text_edit_undo(window, node).expect("提交为一步 undo"));
}

#[test]
fn empty_no_composition_ime_commit_is_a_no_op_over_selection() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "a你",
    );
    crate::text_edit::set_selection(&mut app, window, node, 1, 4).expect("选区");
    assert!(commit_ime(&mut app, window, "", None));
    assert_eq!(
        text_of(&app, window, node),
        "a你",
        "空的无组合提交不得删除选区（与 r1 insert_text 语义一致）"
    );
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 1..4);
    assert!(!snapshot_of(&app, window, node).can_undo, "no-op 不记 undo");
}

#[test]
fn identity_emoji_composition_caret_and_end_after_commit_are_idempotent() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "ab",
    );
    assert!(preedit(
        &mut app,
        window,
        "🌍a",
        Some(2),
        smallvec::SmallVec::new(),
        None,
    ));
    let composition = composition_of(&app, window, node).expect("组合信息");
    assert_eq!(composition.preedit, 2..7);
    assert_eq!(
        composition.caret, 6,
        "UTF-16 caret 2（🌍 两个码元）落在 display 第 4 个字节"
    );
    assert!(commit_ime(&mut app, window, "🌍", None));
    assert_eq!(text_of(&app, window, node), "ab🌍");
    let generation = snapshot_of(&app, window, node).generation;
    assert!(dispatch_ime(&mut app, window, ImeEvent::End));
    let snapshot = snapshot_of(&app, window, node);
    assert_eq!(snapshot.generation, generation, "提交后补 End 必须幂等");
    assert_eq!(text_of(&app, window, node), "ab🌍");
}

fn placeholder_spec(style: TextStyle, multiline: bool) -> EditableSpec {
    EditableSpec {
        placeholder: smol_str::SmolStr::new_static("Type here"),
        ..spec(style, multiline)
    }
}

fn node_origin(app: &App, window: WindowId, node: NodeId) -> LogicalPoint {
    let tree = app.window(window).expect("窗口存在").tree();
    let bounds = tree.get(node).expect("节点存在").layout().bounds;
    let content_offset = tree.content_offset(node);
    LogicalPoint::new(
        bounds.origin.x + content_offset.x,
        bounds.origin.y + content_offset.y,
    )
}

#[test]
fn empty_editable_caret_rect_covers_the_first_line_box() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        Some(120.0),
        "",
    );
    let metrics = app.text_edit_metrics(window, node).expect("度量");
    let caret = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_caret_rect(node, 0)
        .expect("空文本 caret 几何");
    assert!(
        (caret.size.height - metrics.line_height).abs() <= px(0.5),
        "空文本 caret 高度必须等于行盒：{caret:?} vs {metrics:?}"
    );
    assert!(
        caret.origin.y.abs() <= px(0.5),
        "空文本 caret 必须落在首行行盒顶部：{caret:?}"
    );
    assert!(caret.size.width >= px(1.0), "caret 宽度至少 1：{caret:?}");
}

#[test]
fn placeholder_line_baseline_matches_the_first_text_baseline() {
    let (_rt, mut app, window) = new_app();
    for (size, multiline, width) in [
        (12.0, false, 160.0),
        (16.0, false, 160.0),
        (30.0, false, 200.0),
        (16.0, true, 60.0),
    ] {
        let node = mount(
            &mut app,
            window,
            placeholder_spec(TextStyle::new(px(size)), multiline),
            Some(width),
        );
        refresh(&mut app, window);
        layout(&mut app, window);
        text_layout(&mut app, window);

        let metrics = app.text_edit_metrics(window, node).expect("度量");
        let baseline = metrics.first_baseline.expect("首行基线");
        let placeholder_baseline = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .editable(node)
            .expect("可编辑槽位")
            .visible_placeholder_line()
            .expect("空文本必须缓存 placeholder 行")
            .first_baseline
            .expect("placeholder 行必须有基线");
        assert!(
            (placeholder_baseline - baseline).abs() <= px(0.5),
            "size {size} multiline {multiline}: placeholder 基线 {placeholder_baseline} 必须等于首行基线 {baseline}"
        );

        if multiline {
            app.text_edit_set_value(window, node, "a\nb\nc")
                .expect("写入三行");
            refresh(&mut app, window);
            layout(&mut app, window);
            text_layout(&mut app, window);
            assert!(
                app.window(window)
                    .expect("窗口存在")
                    .tree()
                    .editable(node)
                    .expect("可编辑槽位")
                    .visible_placeholder_line()
                    .is_none(),
                "有提交文本时不得显示 placeholder"
            );

            app.text_edit_set_value(window, node, "").expect("清空");
            refresh(&mut app, window);
            layout(&mut app, window);
            text_layout(&mut app, window);
            let metrics = app.text_edit_metrics(window, node).expect("度量");
            let baseline = metrics.first_baseline.expect("首行基线");
            let placeholder_baseline = app
                .window(window)
                .expect("窗口存在")
                .tree()
                .editable(node)
                .expect("可编辑槽位")
                .visible_placeholder_line()
                .expect("清空后 placeholder 必须恢复")
                .first_baseline
                .expect("placeholder 行必须有基线");
            assert!(
                (placeholder_baseline - baseline).abs() <= px(0.5),
                "清空后 placeholder 基线 {placeholder_baseline} 必须等于首行基线 {baseline}"
            );
        }
    }
}

#[test]
fn cross_line_selection_rects_cover_each_wrapped_line() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        Some(40.0),
    );
    app.text_edit_set_value(window, node, "alpha beta gamma delta")
        .expect("写入");
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    let metrics = app.text_edit_metrics(window, node).expect("度量");
    assert!(metrics.line_count >= 3, "fixture 必须折成多行：{metrics:?}");
    assert!(crate::text_edit::select_all(&mut app, window, node).expect("全选"));
    let display_len = app
        .text_edit_snapshot(window, node)
        .expect("快照")
        .selection
        .range()
        .end;
    let rects = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .text_selection_rects(node, 0, display_len)
        .expect("选区几何");
    assert_eq!(
        rects.len(),
        metrics.line_count,
        "跨行选区每行一个矩形：{rects:?}"
    );
    let mut previous_y = px(-1.0);
    for rect in &rects {
        assert!(
            rect.size.width > LogicalPx::ZERO,
            "每行矩形必须有宽度：{rect:?}"
        );
        assert!(
            rect.origin.y > previous_y,
            "选区矩形 y 必须逐行递增：{rects:?}"
        );
        previous_y = rect.origin.y;
    }
}

#[test]
fn composition_underline_rects_sit_below_the_baseline_with_clause_thickness() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        Some(160.0),
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("光标");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(2),
        smallvec::smallvec![0..1, 1..2],
        None,
    ));
    refresh(&mut app, window);
    layout(&mut app, window);
    text_layout(&mut app, window);

    let metrics = app.text_edit_metrics(window, node).expect("度量");
    let baseline = metrics.first_baseline.expect("首行基线");
    let tree = app.window(window).expect("窗口存在").tree();
    let shaped = tree.text_slot(node).expect("文本槽位").shaped();
    let width = tree.text_layout_width(node).expect("布局宽度");
    let ranges = tree
        .editable(node)
        .expect("可编辑槽位")
        .composition_ranges()
        .expect("组合 clauses")
        .to_vec();
    assert_eq!(ranges.len(), 2, "两个 clause");
    for (index, range) in ranges.iter().enumerate() {
        let thickness = if index % 2 == 0 { px(1.0) } else { px(1.5) };
        let rects = shaped.composition_underline_rects(
            width,
            crate::text::TextAlign::Start,
            range.clone(),
            px(16.0),
            thickness,
        );
        assert_eq!(rects.len(), 1, "单行 clause 一个下划线：{rects:?}");
        let rect = rects[0];
        assert!(
            (rect.origin.y - (baseline + px(1.6))).abs() <= px(0.5),
            "下划线 y 必须 = baseline + 0.10*font_size：{rect:?} vs {baseline}"
        );
        assert!(
            (rect.size.height - thickness).abs() <= px(1e-3),
            "clause {index} 厚度 {thickness}：{rect:?}"
        );
    }
}

#[test]
fn ime_cursor_area_follows_the_editable_caret_composition_and_blur() {
    let (_rt, mut app, window) = new_app();
    let node = mount(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        Some(160.0),
    );
    app.text_edit_set_value(window, node, "abcd").expect("写入");
    frame(&mut app, window);
    assert_eq!(
        app.ime_cursor_area(window).expect("窗口存在"),
        None,
        "未聚焦可编辑节点时不得写 IME cursor area"
    );

    assert!(app.set_focus(window, Some(node)));
    frame(&mut app, window);
    let area = app
        .ime_cursor_area(window)
        .expect("窗口存在")
        .expect("聚焦后必须有 IME cursor area");
    let expected = caret_screen(&app, window, node, 4);
    assert_caret_area(area, expected);

    crate::text_edit::set_selection(&mut app, window, node, 1, 1).expect("光标");
    frame(&mut app, window);
    let area = app
        .ime_cursor_area(window)
        .expect("窗口存在")
        .expect("光标移动后仍有 IME area");
    assert_caret_area(area, caret_screen(&app, window, node, 1));

    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(1),
        smallvec::SmallVec::new(),
        None,
    ));
    frame(&mut app, window);
    let composition = app
        .text_edit_composition(window, node)
        .expect("组合")
        .expect("组合中");
    let area = app
        .ime_cursor_area(window)
        .expect("窗口存在")
        .expect("组合期必须有 IME area");
    assert_caret_area(area, caret_screen(&app, window, node, composition.caret));

    assert!(app.set_focus(window, None));
    frame(&mut app, window);
    assert_eq!(
        app.ime_cursor_area(window).expect("窗口存在"),
        None,
        "失焦必须清除 IME cursor area"
    );
}

fn caret_screen(app: &App, window: WindowId, node: NodeId, byte_index: usize) -> LogicalRect {
    let tree = app.window(window).expect("窗口存在").tree();
    tree.text_caret_rect(node, byte_index)
        .expect("caret 几何")
        .translate(node_origin(app, window, node))
}

fn assert_caret_area(area: LogicalRect, expected: LogicalRect) {
    assert!(
        (area.origin.x - expected.origin.x).abs() <= px(0.5)
            && (area.origin.y - expected.origin.y).abs() <= px(0.5),
        "IME area 必须等于 caret 屏幕矩形：{area:?} vs {expected:?}"
    );
    assert!(
        (area.size.height - expected.size.height).abs() <= px(0.5),
        "IME area 高度必须等于 caret 高度：{area:?} vs {expected:?}"
    );
}
#[cfg(feature = "test-support")]
fn test_editable_node(
    app: &mut crate::test_support::app::TestApp,
    editable: EditableSpec,
    width: f32,
    value: &str,
) -> NodeId {
    let window = app.window_id();
    let node = {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::editable_text(editable)
            .style(node_style(Some(width)))
            .build(w.tree_mut(), root)
            .expect("挂载可编辑文本")
    };
    app.app_mut()
        .text_edit_set_value(window, node, value)
        .expect("写入");
    node
}

#[cfg(feature = "test-support")]
fn lane_kinds(
    app: &crate::test_support::app::TestApp,
    node: NodeId,
) -> Vec<crate::test_support::record::PrimitiveKind> {
    use crate::test_support::record::PrimitiveKind;

    app.rendered()
        .expect("已提交")
        .primitives
        .iter()
        .filter(|primitive| {
            primitive.node == node
                && matches!(
                    primitive.kind,
                    PrimitiveKind::Quad | PrimitiveKind::GlyphRun
                )
        })
        .map(|primitive| primitive.kind)
        .collect()
}

#[cfg(feature = "test-support")]
fn node_quads(
    app: &crate::test_support::app::TestApp,
    node: NodeId,
) -> Vec<crate::test_support::record::QuadRecord> {
    app.rendered()
        .expect("已提交")
        .quads
        .iter()
        .filter(|quad| quad.node == node)
        .copied()
        .collect()
}

#[cfg(feature = "test-support")]
fn solid_color(red: f32, green: f32, blue: f32, alpha: f32) -> Color {
    Color::Srgb {
        r: red,
        g: green,
        b: blue,
        alpha,
    }
}

#[cfg(feature = "test-support")]
#[test]
fn selection_quad_paints_below_glyphs_and_caret_above_them() {
    use crate::{
        paint::color::ColorResolver,
        test_support::{app::TestApp, record::PrimitiveKind},
    };

    let mut app = TestApp::new();
    let window = app.window_id();
    let selection_color = solid_color(0.2, 0.4, 0.8, 0.5);
    let caret_color = solid_color(1.0, 0.0, 0.0, 1.0);
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.selection_color = selection_color;
    editable.caret_color = caret_color;
    let node = test_editable_node(&mut app, editable, 200.0, "abcd");
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    assert!(crate::text_edit::select_all(app.app_mut(), window, node).expect("全选"));
    app.drain().expect("选区帧");

    assert_eq!(
        lane_kinds(&app, node),
        vec![
            PrimitiveKind::Quad,
            PrimitiveKind::GlyphRun,
            PrimitiveKind::Quad
        ],
        "lane 顺序必须是 选区 Quad → 字形 → caret"
    );
    let quads = node_quads(&app, node);
    assert_eq!(quads.len(), 2, "选区 + caret：{quads:?}");
    assert_eq!(
        quads[0].fill,
        ColorResolver.resolve(selection_color),
        "第一个 Quad 必须是选区"
    );
    assert_eq!(
        quads[1].fill,
        ColorResolver.resolve(caret_color),
        "最后一个 Quad 必须是 caret"
    );
    let metrics = app.app().text_edit_metrics(window, node).expect("度量");
    assert!(
        (quads[1].bounds.size.height - metrics.line_height).abs() <= px(0.5),
        "caret 高度必须等于行盒：{:?} vs {metrics:?}",
        quads[1].bounds
    );
    assert!(
        (quads[1].bounds.origin.x - (quads[0].bounds.origin.x + quads[0].bounds.size.width)).abs()
            <= px(1.0),
        "caret 必须画在选区末尾（focus 端）：{quads:?}"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn empty_editable_paints_the_caret_in_the_line_box() {
    use crate::test_support::{app::TestApp, record::PrimitiveKind};

    let mut app = TestApp::new();
    let window = app.window_id();
    let node =
        test_editable_offset_child(&mut app, spec(TextStyle::new(px(16.0)), false), 120.0, "");
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("帧");

    assert_eq!(
        lane_kinds(&app, node),
        vec![PrimitiveKind::Quad],
        "空文本只画 caret"
    );
    let quads = node_quads(&app, node);
    let caret = quads[0].bounds;
    let metrics = app.app().text_edit_metrics(window, node).expect("度量");
    let tree = app.app().window(window).expect("窗口存在").tree();
    let bounds = tree.get(node).expect("节点").layout().bounds;
    let content_offset = tree.content_offset(node);
    let origin = LogicalPoint::new(
        bounds.origin.x + content_offset.x,
        bounds.origin.y + content_offset.y,
    );
    assert!(
        (caret.size.height - metrics.line_height).abs() <= px(0.5),
        "空文本 caret 高度必须等于行盒：{caret:?} vs {metrics:?}"
    );
    assert!(
        (caret.origin.x - origin.x).abs() <= px(0.5)
            && (caret.origin.y - origin.y).abs() <= px(0.5),
        "空文本 caret 必须位于 content-box 首行行盒起点：{caret:?} vs {origin:?}"
    );
    assert!(
        content_offset.x > LogicalPx::ZERO && content_offset.y > LogicalPx::ZERO,
        "fixture 必须带 border+padding：{content_offset:?}"
    );
    assert!(caret.size.width >= px(1.0), "caret 宽度至少 1：{caret:?}");
}

#[cfg(feature = "test-support")]
#[test]
fn composition_underlines_paint_after_glyphs_and_before_the_caret() {
    use crate::{
        paint::color::ColorResolver,
        test_support::{app::TestApp, record::PrimitiveKind},
    };

    let mut app = TestApp::new();
    let window = app.window_id();
    let composition_color = solid_color(0.0, 0.8, 0.0, 1.0);
    let caret_color = solid_color(1.0, 0.0, 0.0, 1.0);
    let mut editable = spec(TextStyle::new(px(16.0)), false);
    editable.composition_color = composition_color;
    editable.caret_color = caret_color;
    let node = test_editable_offset_child(&mut app, editable, 200.0, "abcd");
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    crate::text_edit::set_selection(app.app_mut(), window, node, 2, 2).expect("光标");
    assert!(preedit(
        app.app_mut(),
        window,
        "ni",
        Some(2),
        smallvec::smallvec![0..1, 1..2],
        None,
    ));
    app.drain().expect("组合帧");

    let kinds = lane_kinds(&app, node);
    assert_eq!(
        kinds,
        vec![
            PrimitiveKind::GlyphRun,
            PrimitiveKind::Quad,
            PrimitiveKind::Quad,
            PrimitiveKind::Quad
        ],
        "组合期顺序必须是 字形 → 两条下划线 → caret"
    );
    let quads = node_quads(&app, node);
    assert_eq!(quads.len(), 3, "{quads:?}");
    assert_eq!(quads[0].fill, ColorResolver.resolve(composition_color));
    assert_eq!(quads[1].fill, ColorResolver.resolve(composition_color));
    assert_eq!(quads[2].fill, ColorResolver.resolve(caret_color));
    let metrics = app.app().text_edit_metrics(window, node).expect("度量");
    let baseline = metrics.first_baseline.expect("首行基线");
    let origin = node_origin(app.app(), window, node);
    let tree = app.app().window(window).expect("窗口存在").tree();
    let ranges = tree
        .editable(node)
        .expect("可编辑槽位")
        .composition_ranges()
        .expect("clauses")
        .to_vec();
    for (index, quad) in quads[..2].iter().enumerate() {
        let thickness = if index % 2 == 0 { px(1.0) } else { px(1.5) };
        assert!(
            (quad.bounds.origin.y - (origin.y + baseline + px(1.6))).abs() <= px(0.5),
            "clause {index} 下划线 y = origin + baseline + 0.10*font_size：{:?} vs {origin:?}+{baseline}",
            quad.bounds
        );
        assert!(
            (quad.bounds.size.height - thickness).abs() <= px(1e-3),
            "clause {index} 厚度 {thickness}：{:?}",
            quad.bounds
        );
        let clause_start = tree
            .text_caret_rect(node, ranges[index].start)
            .expect("clause 起点几何")
            .origin
            .x;
        assert!(
            (quad.bounds.origin.x - (origin.x + clause_start)).abs() <= px(1.0),
            "clause {index} 下划线 x 必须 = origin + caret(clause 起点)：{:?} vs {origin:?}+{clause_start}",
            quad.bounds
        );
    }
}

#[cfg(feature = "test-support")]
#[test]
fn placeholder_glyphs_paint_before_the_caret_only_for_empty_text() {
    use crate::test_support::{app::TestApp, record::PrimitiveKind};

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        placeholder_spec(TextStyle::new(px(16.0)), false),
        200.0,
        "",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("帧");
    assert!(
        app.app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .editable(node)
            .expect("可编辑槽位")
            .visible_placeholder_line()
            .is_some(),
        "热身帧后 placeholder 行必须已缓存"
    );
    assert_eq!(
        lane_kinds(&app, node),
        vec![PrimitiveKind::GlyphRun, PrimitiveKind::Quad],
        "空文本：placeholder 字形 → caret"
    );
    let quads = node_quads(&app, node);
    assert_eq!(quads.len(), 1, "只有 caret：{quads:?}");

    app.app_mut()
        .text_edit_set_value(window, node, "abcd")
        .expect("写入");
    app.drain().expect("有字帧");
    let tree = app.app().window(window).expect("窗口存在").tree();
    assert!(
        tree.editable(node)
            .expect("可编辑槽位")
            .visible_placeholder_line()
            .is_none(),
        "有提交文本时必须隐藏 placeholder"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn cross_line_selection_paints_one_quad_per_line_before_glyphs() {
    use crate::{paint::color::ColorResolver, test_support::record::PrimitiveKind};

    let mut app = crate::test_support::app::TestApp::new();
    let window = app.window_id();
    let selection_color = solid_color(0.2, 0.4, 0.8, 0.5);
    let mut editable = spec(TextStyle::new(px(16.0)), true);
    editable.selection_color = selection_color;
    let node = test_editable_node(&mut app, editable, 40.0, "alpha beta gamma delta");
    assert!(app.app_mut().set_focus(window, Some(node)));
    assert!(crate::text_edit::select_all(app.app_mut(), window, node).expect("全选"));
    app.drain().expect("选区帧");

    let metrics = app.app().text_edit_metrics(window, node).expect("度量");
    let line_count = metrics.line_count;
    assert!(line_count >= 3, "fixture 必须折成多行：{metrics:?}");
    let kinds = lane_kinds(&app, node);
    assert_eq!(
        &kinds[..line_count],
        vec![PrimitiveKind::Quad; line_count],
        "跨行选区必须先画每行的选区 Quad：{kinds:?}"
    );
    assert_eq!(
        kinds[line_count],
        PrimitiveKind::GlyphRun,
        "选区之后才是字形：{kinds:?}"
    );
    assert_eq!(
        kinds.last(),
        Some(&PrimitiveKind::Quad),
        "最后是 caret：{kinds:?}"
    );
    let quads = node_quads(&app, node);
    assert_eq!(quads.len(), line_count + 1, "每行选区 + caret：{quads:?}");
    for quad in &quads[..line_count] {
        assert_eq!(quad.fill, ColorResolver.resolve(selection_color));
        assert!(quad.bounds.size.width > LogicalPx::ZERO);
    }
}

#[cfg(feature = "test-support")]
#[test]
fn caret_and_selection_changes_repaint_only_the_editable_lane_without_shaping() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        let node = NodeBuilder::editable_text(spec(TextStyle::new(px(16.0)), false))
            .style(node_style(Some(200.0)))
            .build(w.tree_mut(), root)
            .expect("挂载可编辑文本");
        let _sibling = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(210.0), px(0.0), px(10.0), px(10.0)))
            .paint(crate::paint::style::PaintSpec {
                background: Color::WHITE,
                ..crate::paint::style::PaintSpec::default()
            })
            .build(w.tree_mut(), root)
            .expect("静态兄弟节点");
        node
    };
    app.app_mut()
        .text_edit_set_value(window, node, "abcd")
        .expect("写入");
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("预热帧");
    let base = app.app().shape_count_for_test();

    assert!(press(
        app.app_mut(),
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    assert!(press(
        app.app_mut(),
        window,
        KeyCode::ArrowLeft,
        Modifiers::empty()
    ));
    app.drain().expect("caret 帧");
    assert_eq!(
        app.app().shape_count_for_test(),
        base,
        "caret 移动不得重新 shape"
    );
    assert_eq!(
        app.frame_record().dirty_nodes,
        vec![node],
        "只重绘可编辑节点的 lane"
    );
    assert_eq!(
        app.frame_record().stats.text.expect("文本报告").shaped,
        0,
        "选区/caret 变化不得触发文本 pass 整形"
    );

    assert!(crate::text_edit::select_all(app.app_mut(), window, node).expect("全选"));
    app.drain().expect("选区帧");
    assert_eq!(
        app.app().shape_count_for_test(),
        base,
        "选区变化不得重新 shape"
    );
    assert_eq!(app.frame_record().dirty_nodes, vec![node]);
}

#[cfg(feature = "test-support")]
#[test]
fn masked_caret_and_selection_use_display_coordinates() {
    use crate::test_support::{app::TestApp, record::PrimitiveKind};

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        masked_spec(TextStyle::new(px(16.0)), None),
        200.0,
        "abcd",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    assert!(crate::text_edit::select_all(app.app_mut(), window, node).expect("全选"));
    app.drain().expect("选区帧");

    let snapshot = app.app().text_edit_snapshot(window, node).expect("快照");
    assert_eq!(
        snapshot.selection.range(),
        0..12,
        "掩码 display 每字素一个 3 字节项目符号"
    );
    assert_eq!(
        lane_kinds(&app, node),
        vec![
            PrimitiveKind::Quad,
            PrimitiveKind::GlyphRun,
            PrimitiveKind::Quad
        ]
    );
    let quads = node_quads(&app, node);
    let selection = quads[0].bounds;
    let caret = quads[1].bounds;
    let glyph = app
        .rendered()
        .expect("已提交")
        .primitives
        .iter()
        .find(|primitive| primitive.node == node && primitive.kind == PrimitiveKind::GlyphRun)
        .expect("掩码字形")
        .bounds;
    assert!(
        (selection.origin.x - glyph.origin.x).abs() <= px(1.5)
            && (selection.origin.x + selection.size.width - (glyph.origin.x + glyph.size.width))
                .abs()
                <= px(2.0),
        "选区必须覆盖掩码 display 字形：{selection:?} vs {glyph:?}"
    );
    assert!(
        (caret.origin.x - (selection.origin.x + selection.size.width)).abs() <= px(0.5),
        "caret 必须位于掩码 display 末尾：{caret:?} vs {selection:?}"
    );
    let metrics = app.app().text_edit_metrics(window, node).expect("度量");
    assert!((caret.size.height - metrics.line_height).abs() <= px(0.5));
}
#[cfg(feature = "test-support")]
#[test]
fn focus_changes_toggle_the_caret_and_repaint_the_editable_lane() {
    use crate::test_support::{app::TestApp, record::PrimitiveKind};

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        spec(TextStyle::new(px(16.0)), false),
        200.0,
        "abcd",
    );
    app.drain().expect("未聚焦帧");
    assert_eq!(
        lane_kinds(&app, node),
        vec![PrimitiveKind::GlyphRun],
        "未聚焦不画 caret"
    );

    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    assert_eq!(
        lane_kinds(&app, node),
        vec![PrimitiveKind::GlyphRun, PrimitiveKind::Quad],
        "聚焦后绘制 caret"
    );
    assert_eq!(app.frame_record().dirty_nodes, vec![node]);

    assert!(app.app_mut().set_focus(window, None));
    app.drain().expect("失焦帧");
    assert_eq!(
        lane_kinds(&app, node),
        vec![PrimitiveKind::GlyphRun],
        "失焦后隐藏 caret"
    );
    assert_eq!(
        app.frame_record().dirty_nodes,
        vec![node],
        "失焦必须重绘该节点"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn masked_empty_text_still_shows_the_placeholder() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let mut editable = masked_spec(TextStyle::new(px(16.0)), None);
    editable.placeholder = smol_str::SmolStr::new_static("Password");
    let node = test_editable_node(&mut app, editable, 200.0, "");
    app.drain().expect("帧");
    assert!(
        app.app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .editable(node)
            .expect("可编辑槽位")
            .visible_placeholder_line()
            .is_some(),
        "掩码空文本同样显示 placeholder"
    );
}

#[cfg(feature = "test-support")]
fn offset_node_style(width: f32) -> Style {
    let mut style = Style::default();
    style.align_self(taffy::style::AlignSelf::START);
    style.width(taffy::style::Dimension::length(width));
    style.padding(px(6.0));
    style.border(px(2.0));
    style
}

#[cfg(feature = "test-support")]
fn test_editable_offset_child(
    app: &mut crate::test_support::app::TestApp,
    editable: EditableSpec,
    width: f32,
    value: &str,
) -> NodeId {
    let window = app.window_id();
    let node = {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        let mut parent_style = Style::default();
        parent_style.align_self(taffy::style::AlignSelf::START);
        parent_style.padding(px(20.0));
        let parent = NodeBuilder::boxed()
            .style(parent_style)
            .build(w.tree_mut(), root)
            .expect("父容器");
        NodeBuilder::editable_text(editable)
            .style(offset_node_style(width))
            .build(w.tree_mut(), parent)
            .expect("可编辑子节点")
    };
    app.app_mut()
        .text_edit_set_value(window, node, value)
        .expect("写入");
    node
}

#[cfg(feature = "test-support")]
fn placeholder_spec_text(style: TextStyle, multiline: bool, placeholder: &str) -> EditableSpec {
    EditableSpec {
        placeholder: smol_str::SmolStr::new(placeholder),
        ..spec(style, multiline)
    }
}

#[cfg(feature = "test-support")]
fn glyph_bounds(app: &crate::test_support::app::TestApp, node: NodeId) -> LogicalRect {
    use crate::test_support::record::PrimitiveKind;

    app.rendered()
        .expect("已提交")
        .primitives
        .iter()
        .filter(|primitive| primitive.node == node && primitive.kind == PrimitiveKind::GlyphRun)
        .fold(LogicalRect::default(), |bounds, primitive| {
            bounds.union(primitive.bounds)
        })
}

#[cfg(feature = "test-support")]
#[test]
fn decorations_include_node_origin_and_content_offset() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_offset_child(
        &mut app,
        spec(TextStyle::new(px(16.0)), false),
        180.0,
        "abcd",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    assert!(crate::text_edit::select_all(app.app_mut(), window, node).expect("全选"));
    app.drain().expect("选区帧");

    let tree = app.app().window(window).expect("窗口存在").tree();
    let bounds = tree.get(node).expect("节点").layout().bounds;
    let content_offset = tree.content_offset(node);
    assert!(
        bounds.origin.x > LogicalPx::ZERO && bounds.origin.y > LogicalPx::ZERO,
        "fixture 节点必须非原点：{bounds:?}"
    );
    assert!(
        content_offset.x > LogicalPx::ZERO && content_offset.y > LogicalPx::ZERO,
        "fixture 节点必须带 border+padding：{content_offset:?}"
    );
    let origin = LogicalPoint::new(
        bounds.origin.x + content_offset.x,
        bounds.origin.y + content_offset.y,
    );
    let expected_caret = tree
        .text_caret_rect(node, 4)
        .expect("caret 几何")
        .translate(origin);
    let glyph = glyph_bounds(&app, node);
    let quads = node_quads(&app, node);
    assert_eq!(quads.len(), 2, "选区 + caret：{quads:?}");
    let selection = quads[0].bounds;
    let caret = quads[1].bounds;
    assert!(
        (caret.origin.x - expected_caret.origin.x).abs() <= px(0.5)
            && (caret.origin.y - expected_caret.origin.y).abs() <= px(0.5),
        "caret 必须含节点原点与 content_offset：{caret:?} vs {expected_caret:?}"
    );
    assert!(
        (caret.size.height - expected_caret.size.height).abs() <= px(0.5),
        "caret 高度必须与 tree 几何一致：{caret:?} vs {expected_caret:?}"
    );
    assert!(
        selection.origin.x <= glyph.origin.x + px(1.5)
            && selection.origin.x + selection.size.width
                >= glyph.origin.x + glyph.size.width - px(1.5),
        "选区必须与同节点字形水平对齐：{selection:?} vs {glyph:?}"
    );
    assert!(
        selection.origin.y <= glyph.origin.y + px(1.5)
            && selection.origin.y + selection.size.height
                >= glyph.origin.y + glyph.size.height - px(1.5),
        "选区必须与同节点字形垂直对齐：{selection:?} vs {glyph:?}"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn placeholder_paints_at_the_text_paint_position() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let style = TextStyle::new(px(16.0));
    let node = test_editable_offset_child(
        &mut app,
        placeholder_spec_text(style, false, "abcd"),
        180.0,
        "",
    );
    app.drain().expect("placeholder 帧");
    let placeholder = glyph_bounds(&app, node);
    assert!(
        placeholder.size.width > LogicalPx::ZERO,
        "placeholder 必须画出字形：{placeholder:?}"
    );
    app.app_mut()
        .text_edit_set_value(window, node, "abcd")
        .expect("写入");
    app.drain().expect("文本帧");
    let text = glyph_bounds(&app, node);
    assert!(
        (placeholder.origin.x - text.origin.x).abs() <= px(0.5)
            && (placeholder.origin.y - text.origin.y).abs() <= px(0.5)
            && (placeholder.size.width - text.size.width).abs() <= px(0.5)
            && (placeholder.size.height - text.size.height).abs() <= px(0.5),
        "paint 层 placeholder 必须与同串文本字形位置一致：{placeholder:?} vs {text:?}"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn placeholder_respects_alignment_and_wraps_to_the_content_width() {
    use crate::{test_support::app::TestApp, text::TextAlign};

    let mut app = TestApp::new();
    let window = app.window_id();
    let mut offsets = Vec::new();
    for alignment in [TextAlign::Start, TextAlign::Center, TextAlign::End] {
        let mut style = TextStyle::new(px(16.0));
        style.alignment = alignment;
        let editable = placeholder_spec_text(style.clone(), false, "Hello");
        let reference = {
            let w = app.app_mut().window_mut(window).expect("窗口存在");
            let root = w.tree().root();
            NodeBuilder::text("Hello", style)
                .style(node_style(Some(200.0)))
                .build(w.tree_mut(), root)
                .expect("参考文本")
        };
        let node = {
            let w = app.app_mut().window_mut(window).expect("窗口存在");
            let root = w.tree().root();
            NodeBuilder::editable_text(editable)
                .style(node_style(Some(200.0)))
                .build(w.tree_mut(), root)
                .expect("placeholder 节点")
        };
        app.drain().expect("帧");
        let tree = app.app().window(window).expect("窗口存在").tree();
        let node_origin = {
            let bounds = tree.get(node).expect("节点").layout().bounds;
            let content_offset = tree.content_offset(node);
            bounds.origin.x + content_offset.x
        };
        let reference_origin = tree.get(reference).expect("节点").layout().bounds.origin.x;
        let placeholder_x = glyph_bounds(&app, node).origin.x - node_origin;
        let reference_x = glyph_bounds(&app, reference).origin.x - reference_origin;
        assert!(
            (placeholder_x - reference_x).abs() <= px(0.5),
            "{alignment:?}: placeholder x {placeholder_x} 必须等于同 style 文本 x {reference_x}"
        );
        offsets.push(placeholder_x);
    }
    assert!(
        offsets[0] < offsets[1] && offsets[1] < offsets[2],
        "Start/Center/End 必须产生递增偏移：{offsets:?}"
    );

    let mut multiline = placeholder_spec_text(
        TextStyle::new(px(16.0)),
        true,
        "alpha beta gamma delta epsilon zeta",
    );
    multiline.multiline = true;
    let node = test_editable_node(&mut app, multiline, 60.0, "");
    app.drain().expect("多行 placeholder 帧");
    let lines = app
        .app()
        .window(window)
        .expect("窗口存在")
        .tree()
        .editable(node)
        .expect("可编辑槽位")
        .visible_placeholder_line()
        .expect("placeholder 行")
        .lines
        .len();
    assert!(lines >= 2, "placeholder 必须按内容宽断行：{lines}");
}

#[cfg(feature = "test-support")]
#[test]
fn ime_cursor_area_uses_the_focus_end_of_a_reversed_selection() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        spec(TextStyle::new(px(16.0)), false),
        200.0,
        "abcd",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    crate::text_edit::set_selection(app.app_mut(), window, node, 4, 1).expect("反向选区");
    app.drain().expect("选区帧");

    let area = app
        .app()
        .ime_cursor_area(window)
        .expect("窗口存在")
        .expect("反向选区仍须写 IME area");
    assert_caret_area(area, caret_screen(app.app(), window, node, 1));
    let anchor_caret = caret_screen(app.app(), window, node, 4);
    assert!(
        (area.origin.x - anchor_caret.origin.x).abs() > px(1.0),
        "IME area 必须用 focus 端而不是 anchor 端：{area:?} vs {anchor_caret:?}"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn removing_a_focused_editable_clears_the_ime_cursor_area() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        spec(TextStyle::new(px(16.0)), false),
        200.0,
        "abcd",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    assert!(
        app.app()
            .ime_cursor_area(window)
            .expect("窗口存在")
            .is_some(),
        "前置：聚焦后必须有 IME area"
    );

    app.app_mut().remove_node(window, node).expect("删除节点");
    app.drain().expect("删除帧");
    assert_eq!(
        app.app().ime_cursor_area(window).expect("窗口存在"),
        None,
        "删除聚焦可编辑节点后 IME area 必须清除"
    );
    assert_eq!(
        app.app()
            .window(window)
            .expect("窗口存在")
            .ime_cursor_node(),
        None,
        "删除后托管 request 不得残留"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn text_edit_set_placeholder_invalidates_the_cached_placeholder_line() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        placeholder_spec_text(TextStyle::new(px(16.0)), false, "old"),
        200.0,
        "",
    );
    app.drain().expect("初始 placeholder 帧");
    let line_text = |app: &TestApp| {
        app.app()
            .window(app.window_id())
            .expect("窗口存在")
            .tree()
            .editable(node)
            .expect("可编辑槽位")
            .visible_placeholder_line()
            .map(|line| line.text.clone())
    };
    assert_eq!(line_text(&app).as_deref(), Some("old"));
    let before = glyph_bounds(&app, node);

    assert!(
        app.app_mut()
            .text_edit_set_placeholder(window, node, "new hint")
            .expect("setter"),
        "placeholder 变化必须报告 changed"
    );
    app.drain().expect("placeholder 更新帧");
    assert_eq!(
        line_text(&app).as_deref(),
        Some("new hint"),
        "缓存行必须失效重建"
    );
    let after = glyph_bounds(&app, node);
    assert!(
        (after.size.width - before.size.width).abs() > px(1.0),
        "绘制宽度必须跟随新 placeholder：{before:?} -> {after:?}"
    );

    assert!(
        app.app_mut()
            .text_edit_set_placeholder(window, node, "")
            .expect("setter"),
        "置空必须报告 changed"
    );
    app.drain().expect("置空帧");
    assert_eq!(line_text(&app), None, "置空后不得再绘制旧缓存行");
    assert!(
        lane_kinds(&app, node).is_empty(),
        "空 placeholder + 未聚焦不得有图元：{:?}",
        lane_kinds(&app, node)
    );
    assert!(
        !app.app_mut()
            .text_edit_set_placeholder(window, node, "")
            .expect("幂等"),
        "重复置空不得报告 changed"
    );
    assert!(
        app.app_mut()
            .text_edit_set_placeholder(window, node, "again")
            .expect("setter")
    );
    app.drain().expect("再次设置帧");
    assert_eq!(line_text(&app).as_deref(), Some("again"));
}

#[cfg(feature = "test-support")]
#[test]
fn text_edit_set_placeholder_while_text_exists_shows_after_clearing() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(&mut app, spec(TextStyle::new(px(16.0)), false), 200.0, "x");
    app.drain().expect("有字帧");
    assert!(
        app.app_mut()
            .text_edit_set_placeholder(window, node, "hint")
            .expect("setter")
    );
    app.drain().expect("设置帧");
    assert!(
        app.app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .editable(node)
            .expect("可编辑槽位")
            .visible_placeholder_line()
            .is_none(),
        "有文本时不得显示 placeholder"
    );
    app.app_mut()
        .text_edit_set_value(window, node, "")
        .expect("清空");
    app.drain().expect("清空帧");
    let line = app
        .app()
        .window(window)
        .expect("窗口存在")
        .tree()
        .editable(node)
        .expect("可编辑槽位")
        .visible_placeholder_line()
        .cloned()
        .expect("清空后 placeholder 必须出现");
    assert_eq!(line.text.as_str(), "hint");
}

#[cfg(feature = "test-support")]
#[test]
fn text_edit_color_setters_repaint_without_reshaping() {
    use crate::{paint::color::ColorResolver, test_support::app::TestApp};

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        spec(TextStyle::new(px(16.0)), false),
        200.0,
        "abcd",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    assert!(crate::text_edit::select_all(app.app_mut(), window, node).expect("全选"));
    app.drain().expect("预热帧");
    let base = app.app().shape_count_for_test();

    let caret_color = solid_color(0.0, 1.0, 0.0, 1.0);
    assert!(
        app.app_mut()
            .text_edit_set_caret_color(window, node, caret_color)
            .expect("caret color")
    );
    app.drain().expect("caret 颜色帧");
    let quads = node_quads(&app, node);
    assert_eq!(
        quads.last().expect("caret").fill,
        ColorResolver.resolve(caret_color)
    );

    let selection_color = solid_color(0.0, 0.0, 1.0, 0.5);
    assert!(
        app.app_mut()
            .text_edit_set_selection_color(window, node, selection_color)
            .expect("selection color")
    );
    app.drain().expect("选区颜色帧");
    let quads = node_quads(&app, node);
    assert_eq!(
        quads.first().expect("选区").fill,
        ColorResolver.resolve(selection_color)
    );

    crate::text_edit::set_selection(app.app_mut(), window, node, 2, 2).expect("光标");
    assert!(preedit(
        app.app_mut(),
        window,
        "ni",
        Some(2),
        smallvec::smallvec![0..1, 1..2],
        None,
    ));
    app.drain().expect("组合帧");
    let base_after_composition = app.app().shape_count_for_test();
    let composition_color = solid_color(1.0, 1.0, 0.0, 1.0);
    assert!(
        app.app_mut()
            .text_edit_set_composition_color(window, node, composition_color)
            .expect("composition color")
    );
    app.drain().expect("组合颜色帧");
    let quads = node_quads(&app, node);
    let composition_quads: Vec<_> = quads
        .iter()
        .filter(|quad| quad.fill == ColorResolver.resolve(composition_color))
        .collect();
    assert_eq!(
        composition_quads.len(),
        2,
        "两条 clause 下划线必须换色：{quads:?}"
    );
    assert_eq!(
        app.app().shape_count_for_test(),
        base_after_composition,
        "颜色 setter 不得重新 shape"
    );
    assert!(
        app.app().shape_count_for_test() > base,
        "组合本身允许一次 shape（前置断言）"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn text_edit_set_placeholder_color_rebuilds_without_reshaping() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        placeholder_spec_text(TextStyle::new(px(16.0)), false, "hint"),
        200.0,
        "",
    );
    app.drain().expect("预热帧");
    let base = app.app().shape_count_for_test();
    let color = solid_color(0.0, 1.0, 0.0, 1.0);
    assert!(
        app.app_mut()
            .text_edit_set_placeholder_color(window, node, color)
            .expect("placeholder color")
    );
    app.drain().expect("颜色帧");
    assert_eq!(
        app.app().shape_count_for_test(),
        base,
        "placeholder 颜色变化必须命中原 shape cache"
    );
    let tree = app.app().window(window).expect("窗口存在").tree();
    let editable = tree.editable(node).expect("可编辑槽位");
    assert_eq!(editable.spec().placeholder_color, color);
    assert!(
        editable.visible_placeholder_line().is_some(),
        "颜色变化后 placeholder 行必须重建并可见"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn text_edit_set_text_style_reshapes_and_rebuilds_the_placeholder_line() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let small = TextStyle::new(px(12.0));
    let node = test_editable_node(
        &mut app,
        placeholder_spec_text(small, false, "abcd"),
        180.0,
        "",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("预热帧");
    let base = app.app().shape_count_for_test();
    let before = app.app().text_edit_metrics(window, node).expect("度量");

    let large = TextStyle::new(px(24.0));
    assert!(
        app.app_mut()
            .text_edit_set_text_style(window, node, large.clone())
            .expect("text style")
    );
    app.drain().expect("样式帧");
    let after = app.app().text_edit_metrics(window, node).expect("度量");
    assert!(
        after.line_height > before.line_height,
        "字号变大后行盒必须变大：{before:?} -> {after:?}"
    );
    assert!(
        app.app().shape_count_for_test() > base,
        "样式变化必须重新 shape"
    );
    let tree = app.app().window(window).expect("窗口存在").tree();
    assert_eq!(tree.text_content(node).expect("文本内容").style, large);
    let editable = tree.editable(node).expect("可编辑槽位");
    let placeholder_baseline = editable
        .visible_placeholder_line()
        .expect("placeholder 行必须重建")
        .first_baseline
        .expect("基线");
    assert!(
        (placeholder_baseline - after.first_baseline.expect("首行基线")).abs() <= px(0.5),
        "样式变化后 placeholder 基线必须跟随新首行基线"
    );
    let quads = node_quads(&app, node);
    let caret = quads.last().expect("caret");
    assert!(
        (caret.bounds.size.height - after.line_height).abs() <= px(0.5),
        "caret 高度必须跟随新行盒：{:?} vs {after:?}",
        caret.bounds
    );
}

#[cfg(feature = "test-support")]
fn test_editable_styled(
    app: &mut crate::test_support::app::TestApp,
    editable: EditableSpec,
    style: Style,
    value: &str,
) -> NodeId {
    let window = app.window_id();
    let node = {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::editable_text(editable)
            .style(style)
            .build(w.tree_mut(), root)
            .expect("可编辑文本")
    };
    app.app_mut()
        .text_edit_set_value(window, node, value)
        .expect("写入");
    node
}

#[cfg(feature = "test-support")]
fn reference_text_node(
    app: &mut crate::test_support::app::TestApp,
    text: &str,
    style: TextStyle,
    node_style: Style,
) -> NodeId {
    let window = app.window_id();
    let w = app.app_mut().window_mut(window).expect("窗口存在");
    let root = w.tree().root();
    NodeBuilder::text(text, style)
        .style(node_style)
        .build(w.tree_mut(), root)
        .expect("参考文本")
}

#[cfg(feature = "test-support")]
fn placeholder_alignment_metrics(
    app: &crate::test_support::app::TestApp,
    window: WindowId,
    node: NodeId,
    reference: NodeId,
) -> (usize, usize, LogicalPx, LogicalPx) {
    let tree = app.app().window(window).expect("窗口存在").tree();
    let placeholder_lines = tree
        .editable(node)
        .expect("可编辑槽位")
        .visible_placeholder_line()
        .expect("placeholder 行")
        .lines
        .len();
    let reference_lines = tree.text_line(reference).expect("参考行").lines.len();
    let placeholder_x = glyph_bounds(app, node).origin.x - node_origin(app.app(), window, node).x;
    let reference_bounds = tree.get(reference).expect("参考节点").layout().bounds;
    let reference_x = glyph_bounds(app, reference).origin.x
        - reference_bounds.origin.x
        - tree.content_offset(reference).x;
    (
        placeholder_lines,
        reference_lines,
        placeholder_x,
        reference_x,
    )
}

#[cfg(feature = "test-support")]
fn lane_has_glyphs(app: &crate::test_support::app::TestApp, node: NodeId) -> bool {
    use crate::test_support::record::PrimitiveKind;

    app.rendered().is_some_and(|rendered| {
        rendered
            .primitives
            .iter()
            .any(|primitive| primitive.node == node && primitive.kind == PrimitiveKind::GlyphRun)
    })
}

#[cfg(feature = "test-support")]
fn glyph_run_color(
    app: &crate::test_support::app::TestApp,
    window: WindowId,
    node: NodeId,
) -> Option<Color> {
    use crate::paint::primitive::Primitive;

    let target = app.app().window(window)?;
    let lane = target.tree().lane_of(node)?;
    target
        .scene()
        .lane_primitives(lane)?
        .iter()
        .find_map(|primitive| match primitive {
            Primitive::GlyphRun(run) => Some(run.color),
            _ => None,
        })
}

#[cfg(feature = "test-support")]
#[test]
fn placeholder_rebuilds_when_the_node_content_width_changes() {
    use crate::{test_support::app::TestApp, text::TextAlign};

    let mut app = TestApp::new();
    let window = app.window_id();
    let placeholder = "alpha beta gamma delta epsilon zeta";
    let mut style = TextStyle::new(px(16.0));
    style.alignment = TextAlign::Center;
    let mut editable = placeholder_spec_text(style.clone(), true, placeholder);
    editable.multiline = true;
    let wide = node_style(Some(200.0));
    let node = test_editable_styled(&mut app, editable, wide.clone(), "");
    let reference = reference_text_node(&mut app, placeholder, style, wide);
    app.drain().expect("初始帧");
    let initial = placeholder_alignment_metrics(&app, window, node, reference);
    assert!(initial.0 >= 2, "fixture 在 200px 必须折行：{initial:?}");
    assert_eq!(initial.0, initial.1, "初始行数必须与参考一致");
    assert!(
        (initial.2 - initial.3).abs() <= px(0.5),
        "初始 Center 偏移必须与参考一致：{initial:?}"
    );

    let narrow = node_style(Some(60.0));
    {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        w.tree_mut()
            .set_style(node, narrow.clone())
            .expect("收窄可编辑");
        w.tree_mut().set_style(reference, narrow).expect("收窄参考");
    }
    app.app_mut().request_frame();
    app.drain().expect("收窄帧");
    let after = placeholder_alignment_metrics(&app, window, node, reference);
    assert!(
        after.0 > initial.0,
        "内容宽 200→60 后 placeholder 必须重排：{initial:?} -> {after:?}"
    );
    assert_eq!(after.0, after.1, "收窄后行数必须与参考一致：{after:?}");
    assert!(
        (after.2 - after.3).abs() <= px(0.5),
        "收窄后 Center 偏移必须与参考一致：{after:?}"
    );

    app.app_mut()
        .text_edit_set_value(window, node, "x")
        .expect("写入");
    app.drain().expect("打字帧");
    app.app_mut()
        .text_edit_set_value(window, node, "")
        .expect("清空");
    app.drain().expect("清空帧");
    let healed = placeholder_alignment_metrics(&app, window, node, reference);
    assert_eq!(healed.0, healed.1, "打字清空后必须自愈：{healed:?}");
    assert!(
        (healed.2 - healed.3).abs() <= px(0.5),
        "打字清空后偏移必须与参考一致：{healed:?}"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn placeholder_follows_a_real_window_resize() {
    use crate::{test_support::app::TestApp, text::TextAlign};

    let mut app = TestApp::with_options(WindowOptions {
        title: smol_str::SmolStr::new_static("resize"),
        size: LogicalSize::new(px(800.0), px(600.0)),
        ..WindowOptions::default()
    });
    let window = app.window_id();
    let placeholder = "alpha beta gamma delta epsilon zeta";
    let mut style = TextStyle::new(px(16.0));
    style.alignment = TextAlign::Center;
    let mut editable = placeholder_spec_text(style.clone(), true, placeholder);
    editable.multiline = true;
    let mut full_width = Style::default();
    full_width.align_self(taffy::style::AlignSelf::START);
    full_width.width(taffy::style::Dimension::percent(1.0));
    let node = test_editable_styled(&mut app, editable, full_width.clone(), "");
    let reference = reference_text_node(&mut app, placeholder, style, full_width);
    app.drain().expect("800px 帧");
    let wide = placeholder_alignment_metrics(&app, window, node, reference);
    assert_eq!(wide.0, wide.1, "800px 行数必须与参考一致：{wide:?}");

    app.app_mut()
        .window_mut(window)
        .expect("窗口存在")
        .set_size(LogicalSize::new(px(300.0), px(600.0)));
    app.app_mut().request_frame();
    app.drain().expect("300px 帧");
    let narrow = placeholder_alignment_metrics(&app, window, node, reference);
    assert_eq!(narrow.0, narrow.1, "300px 行数必须与参考一致：{narrow:?}");
    assert!(
        (narrow.2 - narrow.3).abs() <= px(0.5),
        "300px Center 偏移必须与参考一致：{narrow:?}"
    );
    assert!(
        narrow.0 != wide.0 || (narrow.2 - wide.2).abs() > px(0.5),
        "窗口收窄后 placeholder 必须改变：{wide:?} -> {narrow:?}"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn stale_placeholder_line_is_repainted_after_a_redraw_request() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        placeholder_spec_text(TextStyle::new(px(16.0)), false, "abcd"),
        200.0,
        "",
    );
    app.drain().expect("预热帧");
    {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        let tree = w.tree_mut();
        let width = tree.text_layout_width(node).expect("布局宽度");
        let existing = tree
            .editable(node)
            .expect("可编辑槽位")
            .visible_placeholder_line()
            .expect("placeholder 行")
            .clone();
        let mut stale = (*existing).clone();
        stale.atlas_generation = stale.atlas_generation.wrapping_add(1);
        tree.editable_mut(node)
            .expect("可编辑槽位")
            .set_placeholder_line(std::rc::Rc::new(stale), width);
        tree.mark_paint(node);
    }
    app.app_mut().request_frame();
    app.drain().expect("stale 帧");
    assert!(
        !lane_has_glyphs(&app, node),
        "atlas generation 失效的 placeholder 旧 tile 不得绘制"
    );

    app.drain().expect("自愈帧");
    assert!(
        lane_has_glyphs(&app, node),
        "失效守卫必须请求重绘并在下一帧重建 placeholder"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn placeholder_paint_uses_the_foreground_style_patch_when_present() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let placeholder_color = solid_color(0.0, 1.0, 0.0, 1.0);
    let patch_color = solid_color(0.0, 0.0, 1.0, 1.0);
    let mut editable = placeholder_spec_text(TextStyle::new(px(16.0)), false, "abcd");
    editable.placeholder_color = placeholder_color;
    let plain = test_editable_node(&mut app, editable, 200.0, "");
    let patched = {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::editable_text(placeholder_spec_text(
            TextStyle::new(px(16.0)),
            false,
            "abcd",
        ))
        .style(node_style(Some(200.0)))
        .bind_style(move |_| crate::paint::style::StylePatch {
            foreground: Some(patch_color),
            ..crate::paint::style::StylePatch::default()
        })
        .build(w.tree_mut(), root)
        .expect("带前景 patch 的可编辑文本")
    };
    app.drain().expect("帧");
    assert_eq!(
        glyph_run_color(&app, window, plain),
        Some(placeholder_color),
        "无 patch 时 placeholder 必须用 placeholder_color"
    );
    assert_eq!(
        glyph_run_color(&app, window, patched),
        Some(patch_color),
        "有 foreground patch 时 placeholder 必须用 patch 颜色"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn text_edit_set_text_style_rejects_invalid_styles() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        spec(TextStyle::new(px(16.0)), false),
        200.0,
        "abcd",
    );
    app.drain().expect("预热帧");

    let mut zero_size = TextStyle::new(px(0.0));
    zero_size.color = Color::BLACK;
    assert!(
        matches!(
            app.app_mut().text_edit_set_text_style(window, node, zero_size),
            Err(EngineError::TextEditSpecInvalid { node: reported, .. }) if reported == node
        ),
        "size=0 必须报 TextEditSpecInvalid"
    );
    let mut truncating = TextStyle::new(px(16.0));
    truncating.max_lines = 1;
    assert!(matches!(
        app.app_mut()
            .text_edit_set_text_style(window, node, truncating),
        Err(EngineError::TextEditSpecInvalid { .. })
    ));
    let mut zero_line_height = TextStyle::new(px(16.0));
    zero_line_height.line_height = Some(0.0);
    assert!(matches!(
        app.app_mut()
            .text_edit_set_text_style(window, node, zero_line_height),
        Err(EngineError::TextEditSpecInvalid { .. })
    ));

    let valid = TextStyle::new(px(20.0));
    assert!(
        app.app_mut()
            .text_edit_set_text_style(window, node, valid.clone())
            .expect("合法样式")
    );
    assert_eq!(
        app.app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .editable(node)
            .expect("可编辑槽位")
            .spec()
            .text_style,
        valid
    );
}

#[cfg(feature = "test-support")]
#[test]
fn disabling_ime_clears_the_managed_request_without_a_frame() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let node = test_editable_node(
        &mut app,
        spec(TextStyle::new(px(16.0)), false),
        200.0,
        "abcd",
    );
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    assert!(
        app.app()
            .ime_cursor_area(window)
            .expect("窗口存在")
            .is_some()
    );

    app.app_mut().set_ime_enabled(window, false);
    assert_eq!(
        app.app().ime_cursor_area(window).expect("窗口存在"),
        None,
        "IME 禁用必须立即清托管 request（不依赖帧/文本 pass）"
    );
    assert_eq!(
        app.app()
            .window(window)
            .expect("窗口存在")
            .ime_cursor_node(),
        None
    );
    app.drain().expect("禁用帧");
    assert_eq!(app.app().ime_cursor_area(window).expect("窗口存在"), None);

    app.app_mut().set_ime_enabled(window, true);
    app.drain().expect("重启帧");
    assert!(
        app.app()
            .ime_cursor_area(window)
            .expect("窗口存在")
            .is_some(),
        "重新启用后 IME area 必须恢复"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn hiding_a_focused_editable_clears_the_ime_cursor_area() {
    use crate::test_support::app::TestApp;

    let mut app = TestApp::new();
    let window = app.window_id();
    let (parent, node) = {
        let w = app.app_mut().window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        let parent = NodeBuilder::boxed()
            .style(node_style(None))
            .build(w.tree_mut(), root)
            .expect("父容器");
        let node = NodeBuilder::editable_text(spec(TextStyle::new(px(16.0)), false))
            .style(node_style(Some(200.0)))
            .build(w.tree_mut(), parent)
            .expect("可编辑文本");
        (parent, node)
    };
    app.app_mut()
        .text_edit_set_value(window, node, "abcd")
        .expect("写入");
    assert!(app.app_mut().set_focus(window, Some(node)));
    app.drain().expect("聚焦帧");
    assert!(
        app.app()
            .ime_cursor_area(window)
            .expect("窗口存在")
            .is_some()
    );

    let mut hidden = Style::default();
    hidden.display(taffy::style::Display::None);
    app.app_mut()
        .window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .set_style(parent, hidden)
        .expect("隐藏父容器");
    app.app_mut().request_frame();
    app.drain().expect("隐藏帧");
    assert_eq!(
        app.app().ime_cursor_area(window).expect("窗口存在"),
        None,
        "聚焦但不可见的可编辑节点不得写 IME area"
    );
    assert_eq!(
        app.app()
            .window(window)
            .expect("窗口存在")
            .ime_cursor_node(),
        None
    );

    app.app_mut()
        .window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .set_style(parent, node_style(None))
        .expect("恢复显示");
    app.app_mut().request_frame();
    app.drain().expect("恢复帧");
    assert!(
        app.app()
            .ime_cursor_area(window)
            .expect("窗口存在")
            .is_some(),
        "恢复可见后 IME area 必须重新写入"
    );
}

fn a11y_ids(app: &mut App, window: WindowId, node: NodeId) -> (A11yNodeId, A11yNodeId) {
    let snapshot = a11y_snapshot(app, window);
    let record = snapshot.node_for(node).expect("可编辑节点");
    (
        record.id,
        record.text_run.expect("可编辑节点的 TextRun 子节点"),
    )
}

fn dispatch_a11y(
    app: &mut App,
    window: WindowId,
    target: A11yNodeId,
    action: accesskit::Action,
    data: Option<accesskit::ActionData>,
) {
    crate::a11y::action::dispatch(
        app,
        window,
        accesskit::ActionRequest {
            action,
            target_tree: accesskit::TreeId::ROOT,
            target_node: accesskit::NodeId(target.get()),
            data,
        },
    );
}

fn text_selection_data(run: A11yNodeId, anchor: usize, focus: usize) -> accesskit::ActionData {
    accesskit::ActionData::SetTextSelection(accesskit::TextSelection {
        anchor: accesskit::TextPosition {
            node: accesskit::NodeId(run.get()),
            character_index: anchor,
        },
        focus: accesskit::TextPosition {
            node: accesskit::NodeId(run.get()),
            character_index: focus,
        },
    })
}

#[test]
fn a11y_set_text_selection_selects_by_grapheme_index_on_the_text_run() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "价🌍ab",
    );
    let (input, run) = a11y_ids(&mut app, window, node);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::SetTextSelection,
        Some(text_selection_data(run, 1, 3)),
    );
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        3..8,
        "grapheme 1..3（🌍a）→ display 字节 3..8"
    );

    dispatch_a11y(
        &mut app,
        window,
        run,
        accesskit::Action::SetTextSelection,
        Some(text_selection_data(run, 0, 1)),
    );
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        0..3,
        "动作目标也可以是 TextRun 子节点（解析回同一引擎节点）"
    );

    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::SetTextSelection,
        Some(text_selection_data(run, 3, 1)),
    );
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        3..8,
        "反向选区按引擎语义保留 anchor/focus（range 归一化）"
    );
}

#[test]
fn a11y_replace_selected_text_replaces_the_selection_in_one_undo_step() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "hello",
    );
    crate::text_edit::set_selection(&mut app, window, node, 1, 4).expect("选中 ell");
    let (input, _) = a11y_ids(&mut app, window, node);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::ReplaceSelectedText,
        Some(accesskit::ActionData::Value("i".into())),
    );
    assert_eq!(text_of(&app, window, node), "hio");
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        2..2,
        "替换后 caret 在插入文本之后"
    );
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "hello");
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 1..4);
    assert!(!app.text_edit_undo(window, node).expect("undo 耗尽"));
}

#[test]
fn a11y_replace_selected_text_rejects_readonly_and_disabled_inputs() {
    let (_rt, mut app, window) = new_app();
    let mut locked = spec(TextStyle::new(px(16.0)), false);
    locked.readonly = true;
    let readonly = mount_editable(&mut app, window, locked, None, "abc");
    crate::text_edit::set_selection(&mut app, window, readonly, 0, 1).expect("选中 a");
    let (input, _) = a11y_ids(&mut app, window, readonly);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::ReplaceSelectedText,
        Some(accesskit::ActionData::Value("z".into())),
    );
    assert_eq!(text_of(&app, window, readonly), "abc", "只读输入不得被替换");
    assert!(
        app.a11y_diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            crate::a11y::A11yDiagnostic::ReadOnlyAction {
                node,
                action: A11yAction::ReplaceSelectedText,
            } if *node == readonly
        )),
        "{:?}",
        app.a11y_diagnostics()
    );

    let mut disabled = spec(TextStyle::new(px(16.0)), false);
    disabled.disabled = true;
    let disabled_input = mount(&mut app, window, disabled, None);
    app.text_edit_set_value(window, disabled_input, "abc")
        .expect("写入成功");
    crate::text_edit::set_selection(&mut app, window, disabled_input, 0, 1).expect("选中 a");
    let (input, _) = a11y_ids(&mut app, window, disabled_input);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::ReplaceSelectedText,
        Some(accesskit::ActionData::Value("z".into())),
    );
    assert_eq!(text_of(&app, window, disabled_input), "abc");
    assert!(
        app.a11y_diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            crate::a11y::A11yDiagnostic::DisabledAction {
                node,
                action: A11yAction::ReplaceSelectedText,
            } if *node == disabled_input
        )),
        "{:?}",
        app.a11y_diagnostics()
    );
}

#[test]
fn a11y_replace_selected_text_on_a_password_keeps_the_a11y_text_masked() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        masked_spec(TextStyle::new(px(16.0)), None),
        None,
        "abc",
    );
    crate::text_edit::set_selection(&mut app, window, node, 0, 6).expect("选中两个掩码字符");
    let (input, run) = a11y_ids(&mut app, window, node);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::ReplaceSelectedText,
        Some(accesskit::ActionData::Value("xy".into())),
    );
    assert_eq!(text_of(&app, window, node), "xyc");
    let snapshot = a11y_snapshot(&mut app, window);
    let record = snapshot.node_for(node).expect("密码输入");
    assert_eq!(record.value.as_deref(), Some("•••"));
    let run = snapshot.node(run).expect("TextRun 子节点");
    assert_eq!(run.value.as_deref(), Some("•••"));
    assert!(
        !run.value
            .as_deref()
            .is_some_and(|value| value.contains("xyc"))
    );
    assert_eq!(run.character_lengths, vec![3, 3, 3]);
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "abc");
}

#[test]
fn a11y_set_text_selection_cancels_the_composition_first() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    let (input, run) = a11y_ids(&mut app, window, node);
    assert_eq!(app.focused(window), Some(node), "mount_editable 已聚焦");
    crate::text_edit::set_selection(&mut app, window, node, 2, 2).expect("caret");
    assert!(preedit(
        &mut app,
        window,
        "ni",
        Some(2),
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(snapshot_of(&app, window, node).composing);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::SetTextSelection,
        Some(text_selection_data(run, 0, 1)),
    );
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "SetTextSelection 必须先取消组合");
    assert_eq!(snapshot.selection.range(), 0..1);
    assert_eq!(display_mirror(&app, window, node), "abcd");
}

#[test]
fn a11y_replace_selected_text_sanitizes_and_respects_max_length() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        max_length_spec(TextStyle::new(px(16.0)), Some(3)),
        None,
        "ab",
    );
    crate::text_edit::set_selection(&mut app, window, node, 0, 2).expect("全选");
    let (input, _) = a11y_ids(&mut app, window, node);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::ReplaceSelectedText,
        Some(accesskit::ActionData::Value("x\ty\nzw".into())),
    );
    assert_eq!(
        text_of(&app, window, node),
        "xyz",
        "单行输入剔除控制符后按字素截断到 max_length=3"
    );
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 3..3);
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "ab", "替换是一步 undo");
    assert!(!app.text_edit_undo(window, node).expect("undo 耗尽"));
}

#[test]
fn a11y_replace_selected_text_normalises_crlf_for_multiline() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), true),
        None,
        "aXd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 1, 2).expect("选中 X");
    let (input, _) = a11y_ids(&mut app, window, node);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::ReplaceSelectedText,
        Some(accesskit::ActionData::Value("X\r\nY".into())),
    );
    assert_eq!(
        text_of(&app, window, node),
        "aX\nYd",
        "多行保留换行且 CRLF→LF"
    );
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "aXd");
    assert!(!app.text_edit_undo(window, node).expect("undo 耗尽"));
}

#[test]
fn a11y_replace_selected_text_cancels_the_composition_first() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        None,
        "abcd",
    );
    crate::text_edit::set_selection(&mut app, window, node, 1, 3).expect("选中 bc");
    let (input, _) = a11y_ids(&mut app, window, node);
    assert!(preedit(
        &mut app,
        window,
        "ni",
        None,
        smallvec::SmallVec::new(),
        None,
    ));
    assert!(snapshot_of(&app, window, node).composing);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::ReplaceSelectedText,
        Some(accesskit::ActionData::Value("Z".into())),
    );
    let snapshot = snapshot_of(&app, window, node);
    assert!(!snapshot.composing, "ReplaceSelectedText 必须先取消组合");
    assert_eq!(text_of(&app, window, node), "aZbcd");
    assert_eq!(snapshot.selection.range(), 2..2);
    assert!(app.text_edit_undo(window, node).expect("undo"));
    assert_eq!(text_of(&app, window, node), "abcd");
}

fn pointer_point_at(
    app: &App,
    window: WindowId,
    node: NodeId,
    byte: usize,
) -> (LogicalPoint, LogicalPoint) {
    let caret = app
        .window(window)
        .expect("窗口存在")
        .text_caret_rect(node, byte)
        .expect("caret 几何");
    let origin = bounds(app, window, node).origin;
    let window_point = LogicalPoint::new(
        caret.origin.x + px(0.5),
        caret.origin.y + caret.size.height * 0.5,
    );
    let local = LogicalPoint::new(window_point.x - origin.x, window_point.y - origin.y);
    (window_point, local)
}

#[test]
fn a11y_text_selection_cancels_pointer_drag_and_clears_granularity() {
    let (_rt, mut app, window) = new_app();
    let node = mount_editable(
        &mut app,
        window,
        spec(TextStyle::new(px(16.0)), false),
        Some(300.0),
        "hello world",
    );
    frame(&mut app, window);
    let (pointer, local) = pointer_point_at(&app, window, node, 8);
    assert!(crate::text_edit::pointer_press(
        &mut app,
        window,
        node,
        pointer,
        local,
        2,
        Modifiers::empty()
    ));
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        6..11,
        "双击选中 world（Word 粒度）"
    );
    assert!(
        crate::text_edit::text_pointer_press(&app, window).is_some(),
        "双击后必须存在拖拽会话"
    );

    let (input, run) = a11y_ids(&mut app, window, node);
    dispatch_a11y(
        &mut app,
        window,
        input,
        accesskit::Action::SetTextSelection,
        Some(text_selection_data(run, 6, 11)),
    );
    assert_eq!(snapshot_of(&app, window, node).selection.range(), 6..11);
    assert!(
        crate::text_edit::text_pointer_press(&app, window).is_none(),
        "a11y 动作改变选区后不得残留拖拽态"
    );

    let (pointer, local) = pointer_point_at(&app, window, node, 2);
    assert!(crate::text_edit::pointer_press(
        &mut app,
        window,
        node,
        pointer,
        local,
        1,
        Modifiers::SHIFT
    ));
    assert_eq!(
        snapshot_of(&app, window, node).selection.range(),
        2..6,
        "a11y 同范围选择必须把粒度刷回 Cluster（Word 残留会得到 2..11）"
    );
}

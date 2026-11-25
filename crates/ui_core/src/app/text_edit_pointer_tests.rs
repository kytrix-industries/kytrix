use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use smol_str::SmolStr;

use crate::{
    a11y::A11yRole,
    event::{Action, Event, InputEvent, InputOutcome, Modifiers, PointerButton},
    interaction::{ScrollAxis, ScrollConfig},
    math::{LogicalPoint, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        color::Color,
        style::{Clip, Cursor, Style, StylePatch},
        style_builder::StyleBuilder,
    },
    test_support::app::TestApp,
    text::TextStyle,
    text_edit::{EditableSpec, TextSelection},
    window::FrameOutcome,
};

fn spec(multiline: bool) -> EditableSpec {
    EditableSpec {
        role: if multiline {
            A11yRole::MultilineTextInput
        } else {
            A11yRole::TextInput
        },
        label: SmolStr::new_static(""),
        placeholder: SmolStr::new_static(""),
        multiline,
        masked: false,
        readonly: false,
        disabled: false,
        max_length: None,
        submit: None,
        text_style: TextStyle::new(px(16.0)),
        caret_color: Color::BLACK,
        selection_color: Color::BLACK,
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    }
}

fn node_style(width: Option<f32>) -> Style {
    let mut style = Style::default();
    style.align_self(taffy::style::AlignSelf::START);
    style.flex(0.0, 0.0, taffy::style::Dimension::auto());
    if let Some(width) = width {
        style.width(taffy::style::Dimension::length(width));
    }
    style
}

fn frame(app: &mut TestApp) {
    let _ = app.drain().expect("帧");
}

fn mount(app: &mut TestApp, editable: EditableSpec, width: Option<f32>, value: &str) -> NodeId {
    let id = app.window_id();
    let node = {
        let target = app.app_mut().window_mut(id).expect("窗口存在");
        let root = target.tree().root();
        NodeBuilder::editable_text(editable)
            .style(node_style(width))
            .build(target.tree_mut(), root)
            .expect("挂载可编辑文本")
    };
    app.app_mut()
        .text_edit_set_value(id, node, value)
        .expect("写入文本");
    node
}

fn mount_in(
    app: &mut TestApp,
    parent: NodeId,
    editable: EditableSpec,
    width: Option<f32>,
    value: &str,
) -> NodeId {
    let id = app.window_id();
    let node = {
        let target = app.app_mut().window_mut(id).expect("窗口存在");
        NodeBuilder::editable_text(editable)
            .style(node_style(width))
            .build(target.tree_mut(), parent)
            .expect("挂载可编辑文本")
    };
    app.app_mut()
        .text_edit_set_value(id, node, value)
        .expect("写入文本");
    node
}

fn mount_styled(app: &mut TestApp, editable: EditableSpec, style: Style, value: &str) -> NodeId {
    let id = app.window_id();
    let node = {
        let target = app.app_mut().window_mut(id).expect("窗口存在");
        let root = target.tree().root();
        NodeBuilder::editable_text(editable)
            .style(style)
            .build(target.tree_mut(), root)
            .expect("挂载可编辑文本")
    };
    app.app_mut()
        .text_edit_set_value(id, node, value)
        .expect("写入文本");
    node
}

fn box_node(app: &mut TestApp, parent: NodeId, style: Style) -> NodeId {
    let id = app.window_id();
    let target = app.app_mut().window_mut(id).expect("窗口存在");
    NodeBuilder::boxed()
        .style(style)
        .build(target.tree_mut(), parent)
        .expect("挂载容器")
}

fn bounds(app: &TestApp, node: NodeId) -> LogicalRect {
    app.app()
        .window(app.window_id())
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点存在")
        .layout()
        .bounds
}

fn selection_of(app: &TestApp, node: NodeId) -> TextSelection {
    app.app()
        .text_edit_snapshot(app.window_id(), node)
        .expect("快照")
        .selection
}

fn text_of(app: &TestApp, node: NodeId) -> SmolStr {
    app.app()
        .text_edit_value(app.window_id(), node)
        .expect("真实文本")
}

fn caret_rect(app: &TestApp, node: NodeId, byte: usize) -> LogicalRect {
    app.app()
        .window(app.window_id())
        .expect("窗口存在")
        .text_caret_rect(node, byte)
        .expect("caret 几何")
}

fn point_at(app: &TestApp, node: NodeId, byte: usize) -> LogicalPoint {
    let rect = caret_rect(app, node, byte);
    LogicalPoint::new(
        rect.origin.x + px(0.5),
        rect.origin.y + rect.size.height * 0.5,
    )
}

fn down(
    app: &mut TestApp,
    point: LogicalPoint,
    modifiers: Modifiers,
    at: Instant,
) -> (bool, InputOutcome) {
    let id = app.window_id();
    let event =
        InputEvent::pointer_down(point, PointerButton::Primary, modifiers).with_timestamp(at);
    let (outcome, consumed) = app.app_mut().dispatch_input_tracked(id, event);
    (consumed, outcome)
}

fn up(app: &mut TestApp, point: LogicalPoint, at: Instant) -> (bool, InputOutcome) {
    let id = app.window_id();
    let event = InputEvent::pointer_up(point, PointerButton::Primary, Modifiers::empty())
        .with_timestamp(at);
    let (outcome, consumed) = app.app_mut().dispatch_input_tracked(id, event);
    (consumed, outcome)
}

fn move_to(
    app: &mut TestApp,
    point: LogicalPoint,
    modifiers: Modifiers,
    at: Instant,
) -> (bool, InputOutcome) {
    let id = app.window_id();
    let event = InputEvent::pointer_move(point, modifiers).with_timestamp(at);
    let (outcome, consumed) = app.app_mut().dispatch_input_tracked(id, event);
    (consumed, outcome)
}

fn click(app: &mut TestApp, point: LogicalPoint) -> (bool, bool) {
    let at = Instant::now();
    let (down_consumed, _) = down(app, point, Modifiers::empty(), at);
    let (up_consumed, _) = up(app, point, at);
    (down_consumed, up_consumed)
}

fn click_sequence(app: &mut TestApp, point: LogicalPoint, count: usize) {
    let at = Instant::now();
    for _ in 0..count {
        let _ = down(app, point, Modifiers::empty(), at);
        let _ = up(app, point, at);
    }
}

fn double_click(app: &mut TestApp, point: LogicalPoint) {
    click_sequence(app, point, 2);
}

fn triple_click(app: &mut TestApp, point: LogicalPoint) {
    click_sequence(app, point, 3);
}

fn focused(app: &TestApp) -> Option<NodeId> {
    app.app().focused(app.window_id())
}

fn press_active(app: &TestApp) -> bool {
    crate::text_edit::text_pointer_press(app.app(), app.window_id()).is_some()
}

fn settle(app: &mut TestApp) -> FrameOutcome {
    let window = app.window_id();
    let mut outcome = FrameOutcome::Idle;
    for _ in 0..4 {
        outcome = app
            .app_mut()
            .frame(window)
            .expect("窗口存在")
            .expect("帧成功");
        if matches!(outcome, FrameOutcome::Idle) {
            return outcome;
        }
    }
    outcome
}

#[test]
fn click_sets_the_caret_and_idle_pointer_motion_keeps_the_selection() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(false), Some(200.0), "hello world");
    frame(&mut app);

    let point = point_at(&app, node, 6);
    let id = app.window_id();
    let at = Instant::now();
    let (down_consumed, _) = down(&mut app, point, Modifiers::empty(), at);
    assert!(down_consumed, "可编辑节点必须消费 pointer down");
    assert_eq!(
        app.app_mut().take_capture_requests(),
        vec![(id, true)],
        "按下时请求指针捕获"
    );
    let (up_consumed, _) = up(&mut app, point, at);
    assert!(up_consumed, "可编辑节点必须消费 pointer up");
    assert_eq!(
        app.app_mut().take_capture_requests(),
        vec![(id, false)],
        "抬起时释放指针捕获"
    );
    assert_eq!(focused(&app), Some(node));
    let selection = selection_of(&app, node);
    assert!(selection.is_collapsed());
    assert_eq!(selection.focus().byte, 6);

    let before = selection;
    let (consumed, outcome) = move_to(
        &mut app,
        LogicalPoint::new(px(40.0), px(8.0)),
        Modifiers::empty(),
        Instant::now(),
    );
    assert!(!consumed, "无拖拽的移动不属于编辑会话");
    assert!(outcome.action.is_none());
    assert_eq!(selection_of(&app, node), before, "无按下的移动不得改变选区");

    let (consumed, _) = move_to(
        &mut app,
        LogicalPoint::new(px(60.0), px(9.0)),
        Modifiers::empty(),
        Instant::now(),
    );
    assert!(!consumed);
    assert_eq!(selection_of(&app, node), before);
}

#[test]
fn drag_selects_forward_and_backward_through_the_engine_hit_test() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(false), Some(220.0), "hello world");
    frame(&mut app);

    let at = Instant::now();
    let start = point_at(&app, node, 6);
    let (down_consumed, _) = down(&mut app, start, Modifiers::empty(), at);
    assert!(down_consumed);
    assert!(press_active(&app));

    let target = point_at(&app, node, 11);
    let (move_consumed, _) = move_to(&mut app, target, Modifiers::empty(), at);
    assert!(move_consumed, "拖拽移动必须被编辑会话消费");
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 6);
    assert_eq!(selection.focus().byte, 11);
    assert_eq!(selection.range(), 6..11);

    let (up_consumed, _) = up(&mut app, target, at);
    assert!(up_consumed);
    assert!(!press_active(&app));
    assert_eq!(app.app().pointer_captured(app.window_id()), None);

    let reverse_start = point_at(&app, node, 11);
    let reverse_target = point_at(&app, node, 3);
    let (_, _) = down(
        &mut app,
        reverse_start,
        Modifiers::empty(),
        Instant::now() + Duration::from_secs(2),
    );
    let (_, _) = move_to(
        &mut app,
        reverse_target,
        Modifiers::empty(),
        Instant::now() + Duration::from_secs(2),
    );
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 11);
    assert_eq!(selection.focus().byte, 3);
    assert_eq!(selection.range(), 3..11);
    let _ = up(
        &mut app,
        reverse_target,
        Instant::now() + Duration::from_secs(2),
    );
}

#[test]
fn shift_click_extends_from_the_existing_anchor() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(false), Some(220.0), "hello world");
    frame(&mut app);

    let base = point_at(&app, node, 2);
    let _ = click(&mut app, base);
    assert_eq!(selection_of(&app, node).focus().byte, 2);

    let at = Instant::now() + Duration::from_secs(2);
    let extended = point_at(&app, node, 9);
    let (down_consumed, _) = down(&mut app, extended, Modifiers::SHIFT, at);
    assert!(down_consumed);
    let _ = up(&mut app, extended, at);
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 2);
    assert_eq!(selection.focus().byte, 9);

    let at = Instant::now() + Duration::from_secs(4);
    let backwards = point_at(&app, node, 0);
    let (_, _) = down(&mut app, backwards, Modifiers::SHIFT, at);
    let _ = up(&mut app, backwards, at);
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 2, "Shift+点击保留原 anchor");
    assert_eq!(selection.focus().byte, 0);
    assert_eq!(selection.range(), 0..2);
}

#[test]
fn double_click_selects_words_across_unicode_digits_and_punctuation() {
    let mut app = TestApp::new();
    let node = mount(
        &mut app,
        spec(false),
        Some(400.0),
        "Hi e\u{301}lan 42 foo_bar",
    );
    frame(&mut app);

    let first_word = point_at(&app, node, 3);
    double_click(&mut app, first_word);
    assert_eq!(
        selection_of(&app, node).range(),
        3..9,
        "组合字符属于同一个词"
    );

    let number = point_at(&app, node, 10);
    double_click(&mut app, number);
    assert_eq!(selection_of(&app, node).range(), 10..12, "数字按一个词选中");

    let underscored = point_at(&app, node, 13);
    double_click(&mut app, underscored);
    assert_eq!(selection_of(&app, node).range(), 13..20, "下划线连接词");
}

#[test]
fn triple_click_selects_the_hard_line_even_across_soft_wraps() {
    let mut app = TestApp::new();
    let node = mount(
        &mut app,
        spec(true),
        Some(60.0),
        "aaa bbb ccc\nddd eee\nfff",
    );
    frame(&mut app);
    assert!(
        bounds(&app, node).size.height
            > app
                .app()
                .text_edit_metrics(app.window_id(), node)
                .unwrap()
                .line_height,
        "窄宽度必须产生软换行"
    );

    let wrapped = point_at(&app, node, 8);
    triple_click(&mut app, wrapped);
    let selection = selection_of(&app, node);
    assert_eq!(selection.range(), 0..11, "三击选中整条硬行（跨软换行）");
    assert_eq!(selection.anchor().byte, 0);
    assert_eq!(selection.focus().byte, 11);

    let second = point_at(&app, node, 16);
    triple_click(&mut app, second);
    assert_eq!(selection_of(&app, node).range(), 12..19, "第二行硬行");
}

#[test]
fn double_click_then_drag_extends_by_words() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(false), Some(400.0), "alpha beta gamma delta");
    frame(&mut app);

    let start = point_at(&app, node, 6);
    let at = Instant::now();
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let _ = up(&mut app, start, at);
    let _ = down(&mut app, start, Modifiers::empty(), at);
    assert_eq!(selection_of(&app, node).range(), 6..10, "双击选中 beta");

    let target = point_at(&app, node, 21);
    let (consumed, _) = move_to(&mut app, target, Modifiers::empty(), at);
    assert!(consumed);
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 6, "双击后拖拽保留词扩展粒度");
    assert_eq!(selection.range(), 6..22, "按词扩展到 delta");
    let _ = up(&mut app, target, at);
}

#[test]
fn triple_click_then_drag_extends_by_lines() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(true), Some(400.0), "one\ntwo\nthree\nfour");
    frame(&mut app);

    let start = point_at(&app, node, 4);
    let at = Instant::now();
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let _ = up(&mut app, start, at);
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let _ = up(&mut app, start, at);
    let _ = down(&mut app, start, Modifiers::empty(), at);
    assert_eq!(selection_of(&app, node).range(), 4..7, "三击选中 two");

    let target = point_at(&app, node, 14);
    let (consumed, _) = move_to(&mut app, target, Modifiers::empty(), at);
    assert!(consumed);
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 4, "三击后拖拽保留行扩展粒度");
    assert_eq!(selection.range(), 4..18, "按行扩展到 four");
    let _ = up(&mut app, target, at);
}

#[test]
fn dragging_out_of_the_view_scrolls_the_nearest_ancestor_and_settles() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let mut style = StyleBuilder::new()
        .flex_col()
        .size(px(160.0), px(60.0))
        .build();
    style.clip = Clip::Bounds;
    let root = app.app().window(id).expect("窗口").tree().root();
    let container = box_node(&mut app, root, style);
    let node = mount_in(
        &mut app,
        container,
        spec(true),
        Some(160.0),
        "aaa\nbbb\nccc\nddd\neee\nfff",
    );
    frame(&mut app);

    let handle = app
        .app_mut()
        .make_scroll(
            id,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(20.0),
            },
        )
        .expect("滚动容器");
    let node_bounds = bounds(&app, node);
    app.app_mut()
        .set_scroll_content(
            &handle,
            LogicalSize::new(px(160.0), node_bounds.size.height),
        )
        .expect("滚动内容");
    frame(&mut app);

    let start = point_at(&app, node, 0);
    let at = Instant::now();
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let target = LogicalPoint::new(start.x, px(100.0));
    let (consumed, _) = move_to(&mut app, target, Modifiers::empty(), at);
    assert!(consumed);

    let offset = app.app().scroll_state(&handle).expect("滚动状态").offset;
    assert!(
        offset.y > px(0.0),
        "拖出可视区必须滚动最近祖先 scroll 容器（offset={offset:?}）"
    );
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 0);
    assert_eq!(
        selection.focus().byte,
        text_of(&app, node).len(),
        "拖到视口外后 focus 落在文本末尾"
    );

    let _ = up(&mut app, target, at);
    assert!(!press_active(&app));
    assert_eq!(app.app().pointer_captured(id), None);
    assert!(
        matches!(settle(&mut app), FrameOutcome::Idle),
        "pointer up 之后不得持续出帧"
    );
}

#[test]
fn ancestor_scroll_drag_keeps_the_anchor_and_focus_bytes() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let mut style = StyleBuilder::new()
        .flex_col()
        .size(px(240.0), px(240.0))
        .build();
    style.clip = Clip::Bounds;
    let root = app.app().window(id).expect("窗口").tree().root();
    let container = box_node(&mut app, root, style);
    let _top = box_node(
        &mut app,
        container,
        StyleBuilder::new()
            .flex_none()
            .size(px(240.0), px(100.0))
            .build(),
    );
    let node = mount_in(
        &mut app,
        container,
        spec(true),
        Some(300.0),
        "aaaa\nbbbb\ncccc\ndddd\neeee\nffff\ngggg\nhhhh\niiii\njjjj\nkkkk\nllll",
    );
    let bottom = box_node(
        &mut app,
        container,
        StyleBuilder::new()
            .flex_none()
            .size(px(240.0), px(200.0))
            .build(),
    );
    frame(&mut app);

    let handle = app
        .app_mut()
        .make_scroll(
            id,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(20.0),
            },
        )
        .expect("滚动容器");
    let child_bottom = bottom;
    let content_height = bounds(&app, child_bottom).max().y - bounds(&app, container).origin.y;
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(240.0), content_height))
        .expect("滚动内容");
    app.app_mut()
        .scroll_to(&handle, LogicalPoint::new(px(0.0), px(260.0)))
        .expect("滚动");
    frame(&mut app);
    let scrolled = app.app().scroll_state(&handle).expect("滚动状态").offset;
    assert!(scrolled.y > px(0.0));

    let anchor_byte = 45;
    let anchor_point = point_at(&app, node, anchor_byte);
    assert!(
        anchor_point.y >= px(0.0) && anchor_point.y < px(240.0),
        "锚点必须落在视口内：{anchor_point:?}"
    );
    let at = Instant::now();
    let (down_consumed, _) = down(&mut app, anchor_point, Modifiers::empty(), at);
    assert!(down_consumed);
    assert_eq!(selection_of(&app, node).focus().byte, anchor_byte);

    let inside_target = point_at(&app, node, 50);
    let (move_consumed, _) = move_to(&mut app, inside_target, Modifiers::empty(), at);
    assert!(move_consumed);
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, anchor_byte);
    assert_eq!(selection.focus().byte, 50, "视口内拖拽按窗口坐标命中");

    let outside = LogicalPoint::new(px(1.0), px(239.0));
    let (move_consumed, _) = move_to(&mut app, outside, Modifiers::empty(), at);
    assert!(move_consumed);
    let selection = selection_of(&app, node);
    assert_eq!(
        selection.anchor().byte,
        anchor_byte,
        "祖先滚动后锚点不得回到句首"
    );
    assert_eq!(
        selection.focus().byte,
        text_of(&app, node).len(),
        "拖到容器底部之外 focus 必须落到文本末尾"
    );
    let _ = up(&mut app, outside, at);

    app.app_mut()
        .scroll_to(&handle, LogicalPoint::ZERO)
        .expect("滚回顶部");
    frame(&mut app);
    assert_eq!(
        app.app().scroll_state(&handle).expect("滚动状态").offset,
        LogicalPoint::ZERO
    );

    let anchor_byte = 9;
    let anchor_point = point_at(&app, node, anchor_byte);
    let at = Instant::now() + Duration::from_secs(2);
    let (down_consumed, _) = down(&mut app, anchor_point, Modifiers::empty(), at);
    assert!(down_consumed);
    let target = LogicalPoint::new(px(1.0), px(239.0));
    let (move_consumed, _) = move_to(&mut app, target, Modifiers::empty(), at);
    assert!(move_consumed);
    let selection = selection_of(&app, node);
    assert_eq!(
        selection.anchor().byte,
        anchor_byte,
        "滚回顶部后锚点仍然正确"
    );
    let line_height = app
        .app()
        .text_edit_metrics(id, node)
        .expect("度量")
        .line_height
        .value();
    let node_top = bounds(&app, node).origin.y.value();
    let local = 239.0 - node_top;
    let expected_line = (local / line_height).floor() as usize;
    assert_eq!(
        selection.focus().byte,
        expected_line * 5,
        "滚回顶部后 focus 仍按文本局部坐标映射（line={expected_line}）"
    );
    let _ = up(&mut app, target, at);
}

#[test]
fn pointer_up_and_cancel_clear_the_drag_session() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(false), Some(220.0), "hello world");
    frame(&mut app);

    let start = point_at(&app, node, 2);
    let at = Instant::now();
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let drag_point = point_at(&app, node, 6);
    let _ = move_to(&mut app, drag_point, Modifiers::empty(), at);
    assert!(press_active(&app));
    assert_eq!(app.app().pointer_captured(app.window_id()), Some(node));

    app.dispatch_event(Event::Input(InputEvent::PointerCancel { timestamp: at }));
    assert!(!press_active(&app), "PointerCancel 必须结束拖拽会话");
    assert_eq!(app.app().pointer_captured(app.window_id()), None);

    let selection = selection_of(&app, node);
    let far = point_at(&app, node, 10);
    let _ = move_to(&mut app, far, Modifiers::empty(), at);
    assert_eq!(selection_of(&app, node), selection, "取消后移动不再改选区");

    let _ = down(&mut app, start, Modifiers::empty(), at);
    let _ = move_to(&mut app, drag_point, Modifiers::empty(), at);
    frame(&mut app);
    let (_, _) = down(
        &mut app,
        LogicalPoint::new(px(500.0), px(500.0)),
        Modifiers::empty(),
        at,
    );
    assert!(!press_active(&app), "在其它节点按下必须结束编辑拖拽");
}

#[test]
fn disabled_editable_ignores_pointer_while_readonly_still_selects() {
    let mut app = TestApp::new();
    let mut disabled = spec(false);
    disabled.disabled = true;
    let node = mount(&mut app, disabled, Some(220.0), "readonly text");
    frame(&mut app);

    let point = point_at(&app, node, 2);
    let (down_consumed, up_consumed) = click(&mut app, point);
    assert!(down_consumed, "disabled 可编辑节点也必须截断宿主事件");
    assert!(up_consumed);
    assert_eq!(focused(&app), None, "disabled 节点不可聚焦");
    let selection = selection_of(&app, node);
    assert_eq!(selection.focus().byte, text_of(&app, node).len());

    let mut app = TestApp::new();
    let mut readonly = spec(false);
    readonly.readonly = true;
    let node = mount(&mut app, readonly, Some(220.0), "readonly text");
    frame(&mut app);

    let readonly_point = point_at(&app, node, 2);
    let (down_consumed, _) = click(&mut app, readonly_point);
    assert!(down_consumed);
    assert_eq!(focused(&app), Some(node));
    assert_eq!(selection_of(&app, node).focus().byte, 2);

    let at = Instant::now() + Duration::from_secs(2);
    let _ = down(&mut app, readonly_point, Modifiers::empty(), at);
    let readonly_end = point_at(&app, node, 8);
    let _ = move_to(&mut app, readonly_end, Modifiers::empty(), at);
    assert_eq!(selection_of(&app, node).range(), 2..8, "readonly 允许拖选");
    let _ = up(&mut app, readonly_end, at);

    app.dispatch_event(Event::Input(InputEvent::Text {
        text: SmolStr::new_static("x"),
        timestamp: Instant::now(),
    }));
    assert_eq!(
        text_of(&app, node),
        "readonly text",
        "readonly 拒绝文本变更"
    );
}

#[test]
fn editable_pointer_events_are_consumed_and_skip_host_actions() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let clicks = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&clicks);
    app.app_mut()
        .on_action(Action::custom("host.click"), move |_ctx| {
            seen.set(seen.get() + 1);
        });
    let root = app.app().window(id).expect("窗口").tree().root();
    let parent = {
        let target = app.app_mut().window_mut(id).expect("窗口存在");
        NodeBuilder::boxed()
            .style(StyleBuilder::new().size(px(300.0), px(80.0)).build())
            .on_click(Action::custom("host.click"))
            .build(target.tree_mut(), root)
            .expect("宿主容器")
    };
    let node = mount_in(&mut app, parent, spec(false), Some(200.0), "host text");
    frame(&mut app);
    let _ = app.app_mut().drain_events();

    let point = point_at(&app, node, 2);
    let at = Instant::now();
    app.dispatch_event(Event::Input(
        InputEvent::pointer_down(point, PointerButton::Primary, Modifiers::empty())
            .with_timestamp(at),
    ));
    let queued = app.app_mut().drain_events();
    assert!(
        !queued
            .iter()
            .any(|(_, event)| matches!(event, Event::Input(InputEvent::PointerDown { .. }))),
        "被可编辑节点消费的 pointer down 不得进入宿主事件队列"
    );
    let (up_consumed, outcome) = up(&mut app, point, at);
    assert!(up_consumed);
    assert!(outcome.action.is_none(), "不得派发宿主 click action");
    assert_eq!(clicks.get(), 0);
    assert_eq!(focused(&app), Some(node));
    let queued = app.app_mut().drain_events();
    assert!(
        !queued
            .iter()
            .any(|(_, event)| matches!(event, Event::Input(InputEvent::PointerUp { .. }))),
        "被可编辑节点消费的 pointer up 不得进入宿主事件队列"
    );

    let host_point = LogicalPoint::new(px(280.0), px(70.0));
    let at = Instant::now() + Duration::from_secs(2);
    let (down_consumed, _) = down(&mut app, host_point, Modifiers::empty(), at);
    assert!(!down_consumed, "宿主容器的点击不受可编辑节点影响");
    let (up_consumed, outcome) = up(&mut app, host_point, at);
    assert!(!up_consumed);
    assert_eq!(outcome.action, Some(Action::custom("host.click")));
    assert_eq!(clicks.get(), 1);
}

#[test]
fn pointer_drags_reuse_the_shaped_layout() {
    let mut app = TestApp::new();
    let words: String = (0..60).map(|index| format!("word{index} ")).collect();
    let node = mount(&mut app, spec(true), Some(220.0), words.as_str());
    frame(&mut app);
    let shaped = app.app().shape_count_for_test();
    assert!(shaped > 0);

    let start = point_at(&app, node, 0);
    let at = Instant::now();
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let text_len = text_of(&app, node).len();
    let mut consumed = 0;
    let mut last = start;
    for step in 1..=32 {
        let byte = (step * text_len / 32).min(text_len);
        let point = point_at(&app, node, byte);
        let (hit, _) = move_to(&mut app, point, Modifiers::empty(), at);
        consumed += usize::from(hit);
        last = point;
    }
    assert_eq!(consumed, 32, "拖拽移动全部被消费");
    let _ = up(&mut app, last, at);
    frame(&mut app);
    assert_eq!(
        app.app().shape_count_for_test(),
        shaped,
        "pointer move/拖选不得重新 shape"
    );
    assert!(selection_of(&app, node).range().end > 0, "拖拽必须产生选区");
}

#[test]
fn pointer_drag_moves_stay_within_the_interaction_budget() {
    let mut app = TestApp::new();
    let words: String = (0..400).map(|index| format!("w{index} ")).collect();
    let node = mount(&mut app, spec(true), Some(320.0), words.as_str());
    frame(&mut app);
    let shaped = app.app().shape_count_for_test();
    assert!(shaped > 0);

    let node_bounds = bounds(&app, node);
    let start = point_at(&app, node, 0);
    let at = Instant::now();
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let moves = 2000_usize;
    let left = node_bounds.origin.x + px(2.0);
    let width = node_bounds.size.width - px(4.0);
    let top = node_bounds.origin.y + px(1.0);
    let height = node_bounds.size.height - px(2.0);
    let started = Instant::now();
    let mut consumed = 0;
    for step in 0..moves {
        let x = left + width * ((step % 5) as f32 / 5.0);
        let y = top + height * ((step % 8) as f32 / 8.0);
        let (hit, _) = move_to(&mut app, LogicalPoint::new(x, y), Modifiers::empty(), at);
        consumed += usize::from(hit);
    }
    let elapsed = started.elapsed();
    let per_move_us = elapsed.as_secs_f64() * 1e6 / moves as f64;
    let _ = up(&mut app, LogicalPoint::new(left + width, top + height), at);
    assert_eq!(consumed, moves, "拖拽移动全部被消费");
    assert_eq!(
        app.app().shape_count_for_test(),
        shaped,
        "拖拽期间不得重新 shape"
    );
    assert!(
        per_move_us < 250.0,
        "pointer move 平均 {per_move_us:.1}µs 超出预算（实测约 8µs，回归到 1ms 必须失败）"
    );
    println!(
        "pointer_move_budget: {moves} moves in {:.2}ms ({per_move_us:.1}µs/move)",
        elapsed.as_secs_f64() * 1e3
    );
}

#[test]
fn shift_click_always_extends_inside_the_double_and_triple_click_window() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(false), Some(400.0), "alpha beta gamma");
    frame(&mut app);

    let at = Instant::now();
    let first = point_at(&app, node, 2);
    let _ = down(&mut app, first, Modifiers::empty(), at);
    let _ = up(&mut app, first, at);
    let extend = point_at(&app, node, 9);
    let _ = down(&mut app, extend, Modifiers::SHIFT, at);
    let _ = up(&mut app, extend, at);
    assert_eq!(selection_of(&app, node).range(), 2..9);

    let _ = down(&mut app, extend, Modifiers::SHIFT, at);
    let _ = up(&mut app, extend, at);
    let selection = selection_of(&app, node);
    assert_eq!(
        selection.anchor().byte,
        2,
        "同一双击时间窗内重复 Shift+点击不得升级为选词"
    );
    assert_eq!(selection.range(), 2..9);

    let word = point_at(&app, node, 6);
    for _ in 0..3 {
        let _ = down(&mut app, word, Modifiers::empty(), at);
        let _ = up(&mut app, word, at);
    }
    assert_eq!(selection_of(&app, node).range(), 0..16, "三击选中整行");

    let inner = point_at(&app, node, 6);
    let _ = down(&mut app, inner, Modifiers::SHIFT, at);
    let _ = up(&mut app, inner, at);
    let selection = selection_of(&app, node);
    assert_eq!(selection.anchor().byte, 0, "行基扩展的 anchor 取基区间远侧");
    assert_eq!(
        selection.range(),
        0..6,
        "三击窗口内 Shift+点击仍按行基扩展，而不是再次选中整行"
    );
}

#[test]
fn focus_change_cancels_the_drag_without_consuming_pointer_up() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let node = mount(&mut app, spec(false), Some(220.0), "hello world");
    let other = mount(&mut app, spec(false), Some(220.0), "other field");
    frame(&mut app);

    let at = Instant::now();
    let start = point_at(&app, node, 2);
    let target = point_at(&app, node, 6);
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let _ = move_to(&mut app, target, Modifiers::empty(), at);
    assert_eq!(selection_of(&app, node).range(), 2..6);
    assert_eq!(app.app().pointer_captured(id), Some(node));
    assert!(press_active(&app));

    assert!(app.app_mut().set_focus(id, Some(other)));
    assert!(!press_active(&app), "焦点移走必须结束编辑拖拽");
    assert_eq!(app.app().pointer_captured(id), None, "capture 必须释放");

    let (up_consumed, outcome) = up(&mut app, target, at);
    assert!(!up_consumed, "已取消的拖拽 up 不得被消费");
    assert!(outcome.action.is_none());
    assert_eq!(
        selection_of(&app, node).range(),
        2..6,
        "取消后 up 不得继续扩展旧节点选区"
    );
    assert_eq!(focused(&app), Some(other));
}

#[test]
fn equal_selection_from_plain_click_clears_the_triple_click_granularity() {
    let mut app = TestApp::new();
    let node = mount(&mut app, spec(true), Some(400.0), "hello\n\nworld");
    frame(&mut app);

    let empty = point_at(&app, node, 6);
    triple_click(&mut app, empty);
    assert_eq!(
        selection_of(&app, node).range(),
        6..6,
        "三击空行得到折叠的硬行选区"
    );

    let at = Instant::now() + Duration::from_secs(2);
    let _ = down(&mut app, empty, Modifiers::empty(), at);
    let _ = up(&mut app, empty, at);
    assert_eq!(selection_of(&app, node).range(), 6..6);

    let up_at = at + Duration::from_secs(1);
    let _ = down(&mut app, empty, Modifiers::empty(), up_at);
    let target = point_at(&app, node, 10);
    let (consumed, _) = move_to(&mut app, target, Modifiers::empty(), up_at);
    assert!(consumed);
    assert_eq!(
        selection_of(&app, node).range(),
        6..10,
        "相等选区也必须刷新 anchor_base（普通点击后按簇扩展）"
    );
    let _ = up(&mut app, target, up_at);
}

#[test]
fn triple_click_drag_extends_by_hard_lines_across_soft_wraps() {
    let mut app = TestApp::new();
    let node = mount(
        &mut app,
        spec(true),
        Some(40.0),
        "aaa bbb ccc\nddd eee fff\nggg hhh",
    );
    frame(&mut app);

    let start = point_at(&app, node, 8);
    let at = Instant::now();
    for _ in 0..3 {
        let _ = down(&mut app, start, Modifiers::empty(), at);
        let _ = up(&mut app, start, at);
    }
    assert_eq!(
        selection_of(&app, node).range(),
        0..11,
        "三击选中整条硬行（跨软换行）"
    );

    let _ = down(&mut app, start, Modifiers::empty(), at);
    let target = point_at(&app, node, 12);
    let (consumed, _) = move_to(&mut app, target, Modifiers::empty(), at);
    assert!(consumed);
    assert_eq!(
        selection_of(&app, node).range(),
        0..23,
        "三击后拖拽按硬行扩展（目标硬行 12..23），而不是软行"
    );
    let _ = up(&mut app, target, at);
}

#[test]
fn hover_cursor_is_text_on_editable_and_default_when_disabled() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let node = mount(&mut app, spec(false), Some(220.0), "hover me");
    let mut disabled = spec(false);
    disabled.disabled = true;
    let disabled_node = mount(&mut app, disabled, Some(220.0), "disabled");
    frame(&mut app);
    let _ = app.app_mut().take_cursor_change();

    let enabled_point = point_at(&app, node, 2);
    let _ = move_to(&mut app, enabled_point, Modifiers::empty(), Instant::now());
    assert_eq!(
        app.app_mut().take_cursor_change(),
        Some((id, Cursor::Text)),
        "可编辑 hover 必须是文本光标"
    );

    let disabled_point = point_at(&app, disabled_node, 2);
    let _ = move_to(&mut app, disabled_point, Modifiers::empty(), Instant::now());
    assert_eq!(
        app.app_mut().take_cursor_change(),
        Some((id, Cursor::Default)),
        "disabled 可编辑不得显示文本光标"
    );
}

#[test]
fn pointer_leave_keeps_the_captured_drag_and_node_removal_cancels_it() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let node = mount(&mut app, spec(false), Some(220.0), "hello world");
    frame(&mut app);

    let at = Instant::now();
    let start = point_at(&app, node, 2);
    let target = point_at(&app, node, 6);
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let _ = move_to(&mut app, target, Modifiers::empty(), at);
    app.dispatch_event(Event::Input(InputEvent::PointerLeave {
        position: target,
        modifiers: Modifiers::empty(),
        timestamp: at,
    }));
    assert!(press_active(&app), "capture 期间 leave 不得结束编辑拖拽");
    assert_eq!(app.app().pointer_captured(id), Some(node));

    let _ = app.app_mut().remove_node(id, node);
    assert!(!press_active(&app), "节点删除必须清理编辑拖拽态");
    assert_eq!(app.app().pointer_captured(id), None);
    assert_eq!(focused(&app), None);
}

#[test]
fn padding_and_border_offsets_map_the_pointer_to_text() {
    let mut app = TestApp::new();
    let mut style = StyleBuilder::new().padding(px(6.0)).border(px(2.0)).build();
    style.align_self(taffy::style::AlignSelf::START);
    style.flex(0.0, 0.0, taffy::style::Dimension::auto());
    style.width(taffy::style::Dimension::length(240.0));
    let value = "padding border text";
    let node = mount_styled(&mut app, spec(false), style, value);
    frame(&mut app);

    let start = point_at(&app, node, 3);
    let (down_consumed, _) = down(&mut app, start, Modifiers::empty(), Instant::now());
    assert!(down_consumed);
    assert_eq!(selection_of(&app, node).focus().byte, 3);

    let at = Instant::now() + Duration::from_secs(2);
    let _ = down(&mut app, start, Modifiers::empty(), at);
    let end = point_at(&app, node, value.len());
    let (consumed, _) = move_to(&mut app, end, Modifiers::empty(), at);
    assert!(consumed);
    assert_eq!(
        selection_of(&app, node).range(),
        3..value.len(),
        "padding/border 的 content_offset 不得参与拖选映射错误"
    );
    let _ = up(&mut app, end, at);
}

fn mount_editable_in_host(app: &mut TestApp, host: NodeId, width: f32) -> NodeId {
    let id = app.window_id();
    let target = app.app_mut().window_mut(id).expect("窗口存在");
    let editable = NodeBuilder::editable_text(spec(false))
        .style(node_style(Some(width)))
        .build(target.tree_mut(), host)
        .expect("挂载可编辑文本");
    target.tree_mut().set_style_patch_for_test(
        editable,
        StylePatch {
            interactive: Some(true),
            ..StylePatch::default()
        },
    );
    editable
}

fn mount_press_host(app: &mut TestApp) -> (NodeId, NodeId) {
    let id = app.window_id();
    let host = {
        let target = app.app_mut().window_mut(id).expect("窗口存在");
        let root = target.tree().root();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width(px(220.0))
                    .height(px(44.0))
                    .padding_x(px(12.0))
                    .padding_y(px(8.0))
                    .build(),
            )
            .tab_stop(false)
            .build(target.tree_mut(), root)
            .expect("挂载按压宿主")
    };
    let editable = mount_editable_in_host(app, host, 180.0);
    app.app_mut()
        .window_mut(id)
        .expect("窗口存在")
        .tree_mut()
        .set_press_focus_target(host, editable)
        .expect("登记按压宿主");
    (host, editable)
}

fn chrome_point(app: &TestApp, host: NodeId, from_right: f32) -> LogicalPoint {
    let bounds = bounds(app, host);
    LogicalPoint::new(
        bounds.origin.x + bounds.size.width - px(from_right),
        bounds.origin.y + bounds.size.height * 0.5,
    )
}

#[test]
fn a_press_on_the_host_chrome_places_the_caret_at_the_nearest_text_position() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let (host, editable) = mount_press_host(&mut app);
    app.app_mut()
        .text_edit_set_value(id, editable, "abcdef")
        .expect("写入文本");
    frame(&mut app);
    assert_eq!(
        selection_of(&app, editable).range(),
        6..6,
        "初始 caret 在末尾"
    );

    let at = Instant::now();
    let left = chrome_point(&app, host, 217.0);
    let _ = down(&mut app, left, Modifiers::empty(), at);
    assert_eq!(
        focused(&app),
        Some(editable),
        "按压控件外壳的 padding 必须直接聚焦 editable，不得停留在中间宿主"
    );
    assert_eq!(
        selection_of(&app, editable).range(),
        0..0,
        "左侧 padding 必须把 caret clamp 到最近文本位置（行首）"
    );
    assert!(
        press_active(&app),
        "按压宿主必须进入文本按压态（可继续拖选）"
    );
    let _ = up(&mut app, left, at);

    let right = chrome_point(&app, host, 3.0);
    let _ = down(&mut app, right, Modifiers::empty(), at);
    assert_eq!(
        selection_of(&app, editable).range(),
        6..6,
        "右侧 padding 必须把 caret clamp 到行长（行尾）"
    );
    let _ = up(&mut app, right, at);
    frame(&mut app);
}

#[test]
fn a_press_host_places_the_caret_only_in_the_explicitly_targeted_editable() {
    let mut app = TestApp::new();
    let id = app.window_id();
    let (host, first) = mount_press_host(&mut app);
    let target = mount_editable_in_host(&mut app, host, 80.0);
    app.app_mut()
        .window_mut(id)
        .expect("窗口存在")
        .tree_mut()
        .set_press_focus_target(host, target)
        .expect("登记按压宿主");
    app.app_mut()
        .text_edit_set_value(id, first, "aaa")
        .expect("写入文本");
    app.app_mut()
        .text_edit_set_value(id, target, "bbb")
        .expect("写入文本");
    frame(&mut app);
    assert_eq!(selection_of(&app, first).range(), 3..3);
    assert_eq!(selection_of(&app, target).range(), 3..3);

    let at = Instant::now();
    let left = chrome_point(&app, host, 217.0);
    let _ = down(&mut app, left, Modifiers::empty(), at);
    assert_eq!(
        focused(&app),
        Some(target),
        "宿主必须把焦点交给显式目标，而不是 DFS 里的第一个 editable"
    );
    assert_eq!(
        selection_of(&app, target).range(),
        0..0,
        "显式目标必须按点落 caret"
    );
    assert_eq!(
        selection_of(&app, first).range(),
        3..3,
        "未被指定的 editable 不得被动 caret"
    );
    let _ = up(&mut app, left, at);
}

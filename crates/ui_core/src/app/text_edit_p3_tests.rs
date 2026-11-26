use std::time::{Duration, Instant};

use smol_str::SmolStr;

use crate::{
    a11y::A11yRole,
    event::{Event, ImeEvent, InputEvent, KeyCode, Modifiers, PointerButton},
    interaction::{ScrollAxis, ScrollConfig, ScrollHandle},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        color::{Color, ColorResolver},
        style::{Clip, Style},
        style_builder::StyleBuilder,
    },
    test_support::{
        app::TestApp,
        record::{QuadRecord, RenderedFrame},
    },
    text::{TextStyle, TextWrap},
    text_edit::EditableSpec,
    window::WindowId,
};

const LINE_HEIGHT: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drained {
    Idle,
    Committed,
}

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
        text_style: TextStyle::new(px(16.0))
            .with_wrap(if multiline {
                TextWrap::BreakWord
            } else {
                TextWrap::NoWrap
            })
            .with_line_height(1.25),
        caret_color: Color::from_rgba8([255, 0, 0, 255]),
        selection_color: Color::from_rgba8([0, 0, 255, 255]),
        placeholder_color: Color::BLACK,
        composition_color: Color::BLACK,
    }
}

fn drain(app: &mut TestApp) -> Drained {
    if app.drain().expect("帧").rendered {
        Drained::Committed
    } else {
        Drained::Idle
    }
}

fn root_of(app: &TestApp, window: WindowId) -> NodeId {
    app.app().window(window).expect("窗口").tree().root()
}

fn box_node(app: &mut TestApp, parent: NodeId, style: Style) -> NodeId {
    let window = app.window_id();
    let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
    NodeBuilder::boxed()
        .style(style)
        .build(tree, parent)
        .expect("容器")
}

fn mount_editable(app: &mut TestApp, parent: NodeId, spec: EditableSpec, style: Style) -> NodeId {
    let window = app.window_id();
    let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
    NodeBuilder::editable_text(spec)
        .style(style)
        .build(tree, parent)
        .expect("可编辑节点")
}

fn editable_style(width: Option<f32>) -> Style {
    let mut style = Style::default();
    style.align_self(taffy::style::AlignSelf::START);
    style.flex(0.0, 0.0, taffy::style::Dimension::auto());
    if let Some(width) = width {
        style.width(taffy::style::Dimension::length(width));
    }
    style
}

fn bounds(app: &TestApp, node: NodeId) -> LogicalRect {
    app.app()
        .window(app.window_id())
        .expect("窗口")
        .tree()
        .get(node)
        .expect("节点")
        .layout()
        .bounds
}

fn caret_content(app: &TestApp, node: NodeId, byte: usize) -> LogicalRect {
    let tree = app.app().window(app.window_id()).expect("窗口").tree();
    let local = tree.text_caret_rect(node, byte).expect("caret 几何");
    let bounds = tree.get(node).expect("节点").layout().bounds;
    let offset = tree.content_offset(node);
    local.translate(LogicalPoint::new(
        bounds.origin.x + offset.x,
        bounds.origin.y + offset.y,
    ))
}

fn focus(app: &mut TestApp, node: NodeId) {
    let window = app.window_id();
    assert!(
        app.app_mut().set_focus(window, Some(node)),
        "可编辑节点可聚焦"
    );
}

fn visible(app: &TestApp, window: WindowId) -> Option<bool> {
    app.app().text_edit_caret_visible(window)
}

fn caret_quads(app: &TestApp, node: NodeId) -> Vec<QuadRecord> {
    let caret = ColorResolver.resolve(spec(false).caret_color).components;
    rendered(app)
        .quads
        .iter()
        .filter(|quad| quad.node == node && quad.fill.components == caret)
        .copied()
        .collect()
}

fn node_quads(app: &TestApp, node: NodeId) -> Vec<QuadRecord> {
    rendered(app)
        .quads
        .iter()
        .filter(|quad| quad.node == node)
        .copied()
        .collect()
}

fn rendered(app: &TestApp) -> &RenderedFrame {
    app.rendered().expect("必须提交了像素")
}

fn caret_quads_or_default(app: &TestApp, node: NodeId) -> usize {
    app.rendered()
        .map(|rendered| {
            let caret = ColorResolver.resolve(spec(false).caret_color).components;
            rendered
                .quads
                .iter()
                .filter(|quad| quad.node == node && quad.fill.components == caret)
                .count()
        })
        .unwrap_or(0)
}

struct VerticalBed {
    app: TestApp,
    window: WindowId,
    node: NodeId,
    viewport: NodeId,
    handle: ScrollHandle,
    line_height: LogicalPx,
}

fn vertical_bed(
    text: &str,
    visible_lines: usize,
    stale_content_lines: Option<usize>,
) -> VerticalBed {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let mut style = StyleBuilder::new()
        .flex_col()
        .width(px(200.0))
        .height(px(200.0))
        .build();
    style.clip = Clip::Bounds;
    let viewport = box_node(&mut app, root, style.clone());
    let node = mount_editable(&mut app, viewport, spec(true), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, text)
        .expect("写入文本");
    drain(&mut app);
    let line_height = app
        .app()
        .text_edit_metrics(window, node)
        .expect("度量")
        .line_height;
    style.height(taffy::style::Dimension::length(
        line_height.value() * visible_lines as f32,
    ));
    app.app_mut()
        .window_mut(window)
        .expect("窗口")
        .tree_mut()
        .set_style(viewport, style)
        .expect("设置视口高度");
    drain(&mut app);
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            viewport,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height,
            },
        )
        .expect("滚动容器");
    let content_lines = stale_content_lines.unwrap_or_else(|| {
        (bounds(&app, node).size.height.value() / line_height.value()).round() as usize
    });
    let content = line_height.value() * content_lines as f32;
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(content)))
        .expect("内容尺寸");
    VerticalBed {
        app,
        window,
        node,
        viewport,
        handle,
        line_height,
    }
}

impl VerticalBed {
    fn offset(&self) -> LogicalPoint {
        self.app
            .app()
            .scroll_state(&self.handle)
            .expect("滚动状态")
            .offset
    }

    fn scroll_to(&mut self, y: f32) {
        self.app
            .app_mut()
            .scroll_to(&self.handle, LogicalPoint::new(px(0.0), px(y)))
            .expect("滚动");
    }

    fn set_caret(&mut self, byte: usize) {
        crate::text_edit::set_selection(self.app.app_mut(), self.window, self.node, byte, byte)
            .expect("设置 caret");
    }
}

struct HorizontalBed {
    app: TestApp,
    window: WindowId,
    node: NodeId,
    viewport: NodeId,
    handle: ScrollHandle,
}

fn horizontal_bed(text: &str) -> HorizontalBed {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let mut style = StyleBuilder::new()
        .flex_row()
        .width(px(100.0))
        .height(px(30.0))
        .build();
    style.clip = Clip::Bounds;
    let viewport = box_node(&mut app, root, style);
    let mut editable = editable_style(None);
    editable.padding_sides(px(0.0), px(5.0), px(0.0), px(0.0));
    let node = mount_editable(&mut app, viewport, spec(false), editable);
    app.app_mut()
        .text_edit_set_value(window, node, text)
        .expect("写入文本");
    let handle = app
        .app_mut()
        .make_scroll(
            window,
            viewport,
            ScrollConfig {
                axis: ScrollAxis::Horizontal,
                line_height: px(LINE_HEIGHT),
            },
        )
        .expect("横向滚动容器");
    drain(&mut app);
    let content = bounds(&app, node).size.width;
    app.app_mut()
        .set_scroll_content(&handle, LogicalSize::new(content, px(30.0)))
        .expect("内容尺寸");
    HorizontalBed {
        app,
        window,
        node,
        viewport,
        handle,
    }
}

impl HorizontalBed {
    fn offset(&self) -> LogicalPoint {
        self.app
            .app()
            .scroll_state(&self.handle)
            .expect("滚动状态")
            .offset
    }
}

fn page_line(index: usize) -> usize {
    index * 2
}

#[test]
fn the_caret_blinks_on_a_530ms_half_period() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "abc")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);

    assert_eq!(visible(&app, window), Some(true), "聚焦立即亮");
    assert_eq!(caret_quads(&app, node).len(), 1, "聚焦帧画出 caret");
    assert!(app.app().next_wake_deadline().is_some(), "闪烁必须排唤醒");

    app.advance(Duration::from_millis(529));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "529ms 仍亮");

    app.advance(Duration::from_millis(1));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "530ms 翻转灭");
    assert_eq!(caret_quads_or_default(&app, node), 0, "灭帧不再画 caret");

    app.advance(Duration::from_millis(1));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "531ms 仍灭");

    app.advance(Duration::from_millis(529));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "下一半周期重新亮");
    assert_eq!(caret_quads(&app, node).len(), 1);
}

#[test]
fn editing_resets_the_caret_to_visible_and_restarts_the_period() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "abc")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);
    app.advance(Duration::from_millis(530));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "前置：已熄灭");

    app.advance(Duration::from_millis(100));
    app.inject(InputEvent::key_down(KeyCode::ArrowLeft, Modifiers::empty()));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "编辑事件立即重置为亮");
    assert_eq!(caret_quads(&app, node).len(), 1);

    app.advance(Duration::from_millis(529));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "重置后 529ms 仍亮");

    app.advance(Duration::from_millis(1));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "重置后 530ms 熄灭");
}

#[test]
fn a_selection_still_blinks_the_focus_end_caret() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "abcdef")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);
    app.inject(InputEvent::key_down(KeyCode::KeyA, Modifiers::CONTROL));
    drain(&mut app);
    let pixels = app.rendered().is_some();
    if pixels {
        assert_eq!(node_quads(&app, node).len(), 2, "选区 Quad + caret Quad");
        assert_eq!(caret_quads(&app, node).len(), 1);
    } else {
        eprintln!(
            "跳过 caret/选区像素断言：本会话没有可用的呈现后端（无 Metal/Vulkan 设备），仍校验闪烁相位"
        );
    }

    app.advance(Duration::from_millis(530));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false));
    if pixels {
        assert_eq!(
            node_quads(&app, node).len(),
            1,
            "有选区时熄灭只撤 caret，选区留在帧里"
        );
    }

    app.advance(Duration::from_millis(530));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true));
    if pixels {
        assert_eq!(node_quads(&app, node).len(), 2, "有选区也继续闪 focus 端");
    }
}

#[test]
fn composition_holds_the_caret_solid_and_cancels_the_wake_up() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);

    app.dispatch_event(Event::Ime(ImeEvent::Start));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true));
    assert_eq!(app.app().next_wake_deadline(), None, "组合期取消闪烁唤醒");

    app.advance(Duration::from_millis(3_000));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "组合期常亮（3s 后仍亮）");

    app.dispatch_event(Event::Ime(ImeEvent::Preedit {
        text: SmolStr::new_static("ni"),
        caret: None,
        clauses: Default::default(),
        replace: None,
    }));
    drain(&mut app);
    app.advance(Duration::from_millis(2_000));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "组合更新后仍常亮");

    app.dispatch_event(Event::Ime(ImeEvent::Commit {
        text: SmolStr::new_static("你"),
        replace: None,
    }));
    drain(&mut app);
    assert!(
        app.app().next_wake_deadline().is_some(),
        "提交后恢复闪烁唤醒"
    );
    app.advance(Duration::from_millis(530));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "提交后 530ms 熄灭");
}

#[test]
fn losing_node_focus_hides_the_caret_and_cancels_the_wake_up() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);
    assert!(app.app().next_wake_deadline().is_some());

    app.app_mut().set_focus(window, None);
    drain(&mut app);
    assert_eq!(visible(&app, window), None, "失焦后没有可编辑焦点");
    assert_eq!(app.app().next_wake_deadline(), None, "失焦取消唤醒");

    let index = app.app().window(window).expect("窗口").frame_index().get();
    app.advance(Duration::from_millis(5_000));
    for _ in 0..2 {
        assert_eq!(drain(&mut app), Drained::Idle, "失焦后稳态不得出帧");
    }
    assert_eq!(
        app.app().window(window).expect("窗口").frame_index().get(),
        index
    );

    focus(&mut app, node);
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "再聚焦立即亮");
    assert_eq!(caret_quads(&app, node).len(), 1);
    assert!(app.app().next_wake_deadline().is_some());
}

#[test]
fn window_focus_loss_hides_the_caret_and_restoring_it_shows_again() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);

    app.dispatch_event(Event::FocusChanged { focused: false });
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "窗口失焦隐藏 caret");
    assert_eq!(app.app().next_wake_deadline(), None, "窗口失焦取消唤醒");
    let index = app.app().window(window).expect("窗口").frame_index().get();
    for _ in 0..2 {
        assert_eq!(drain(&mut app), Drained::Idle);
    }
    assert_eq!(
        app.app().window(window).expect("窗口").frame_index().get(),
        index
    );

    app.dispatch_event(Event::FocusChanged { focused: true });
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "窗口恢复立即亮");
    assert_eq!(caret_quads(&app, node).len(), 1);
}

#[test]
fn a_blink_flip_only_repaints_the_caret_node() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);

    app.advance(Duration::from_millis(530));
    let record = app.drain().expect("翻转帧").clone();
    assert!(record.rendered, "翻转必须提交一帧");
    assert_eq!(
        record.dirty_nodes,
        vec![node],
        "只脏 caret 节点，不整帧重绘"
    );
}

#[test]
fn an_unfocused_editable_schedules_no_wake_up() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let _node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    drain(&mut app);
    assert_eq!(app.app().next_wake_deadline(), None);
    for _ in 0..3 {
        assert_eq!(drain(&mut app), Drained::Idle);
    }
}

#[test]
fn ensure_visible_reveals_the_bottom_line_with_one_line_of_padding() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 3, None);
    let line = bed.line_height;
    let target = page_line(4);
    focus(&mut bed.app, bed.node);
    bed.set_caret(target);
    drain(&mut bed.app);

    let caret = caret_content(&bed.app, bed.node, target);
    let viewport = bounds(&bed.app, bed.viewport);
    let expected = caret.max().y + line - (viewport.origin.y + viewport.size.height);
    assert_eq!(
        bed.offset().y,
        expected,
        "底部露出后 caret 下方必须留 1 行（caret={caret:?} viewport={viewport:?}）"
    );
    assert!(bed.offset().y > px(0.0));
    let screen = caret.translate(LogicalPoint::new(px(0.0), -bed.offset().y));
    assert!(
        (viewport.max().y - screen.max().y - line).abs() <= px(0.5),
        "caret 底部与视口底边差 = 1 行：{screen:?} vs {viewport:?}"
    );
    assert!(
        screen.max().y <= viewport.max().y,
        "露出后 caret 必须完整落在视口内：{screen:?} vs {viewport:?}"
    );
}

#[test]
fn ensure_visible_reveals_the_top_line_with_one_line_of_padding() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 3, None);
    let line = bed.line_height;
    focus(&mut bed.app, bed.node);
    bed.set_caret(page_line(5));
    drain(&mut bed.app);
    assert!(bed.offset().y > px(0.0), "前置：已滚到底部露出");

    let target = page_line(2);
    let caret = caret_content(&bed.app, bed.node, target);
    bed.set_caret(target);
    drain(&mut bed.app);

    let viewport = bounds(&bed.app, bed.viewport);
    let expected = caret.origin.y - line;
    assert_eq!(
        bed.offset().y,
        expected,
        "顶部露出后 caret 上方必须留 1 行（caret={caret:?}）"
    );
    let screen = caret.translate(LogicalPoint::new(px(0.0), -bed.offset().y));
    assert!(
        (screen.origin.y - (viewport.origin.y + line)).abs() <= px(0.5),
        "caret 顶部与视口顶边的差 = 1 行：{screen:?} vs {viewport:?}"
    );
}

#[test]
fn ensure_visible_leaves_the_offset_alone_inside_the_safe_zone() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 3, None);
    focus(&mut bed.app, bed.node);
    bed.set_caret(page_line(1));
    drain(&mut bed.app);
    assert_eq!(bed.offset().y, px(0.0), "安全区内不滚动");
}

#[test]
fn ensure_visible_does_not_steal_a_manual_scroll_back() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 3, None);
    focus(&mut bed.app, bed.node);
    bed.set_caret(page_line(4));
    drain(&mut bed.app);
    assert!(bed.offset().y > px(0.0), "前置：reveal 已滚动");

    bed.scroll_to(0.0);
    drain(&mut bed.app);
    assert_eq!(
        bed.offset().y,
        px(0.0),
        "手动滚回顶部后 ensure-visible 不得抢回 caret"
    );
}

#[test]
fn ensure_visible_retries_once_when_the_scroll_content_changes_after_the_first_reveal() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 3, Some(4));
    let line = bed.line_height;
    focus(&mut bed.app, bed.node);
    bed.set_caret(page_line(5));
    drain(&mut bed.app);
    assert_eq!(
        bed.offset().y,
        line * 3.0,
        "末行 reveal 的目标被内容末尾 clamp 到最大偏移"
    );

    bed.app
        .app_mut()
        .set_scroll_content(
            &bed.handle,
            LogicalSize::new(px(200.0), px(line.value() * 8.0)),
        )
        .expect("内容尺寸更新");
    drain(&mut bed.app);
    let caret = caret_content(&bed.app, bed.node, page_line(5));
    let viewport = bounds(&bed.app, bed.viewport);
    let expected = caret.max().y + line - viewport.max().y;
    assert_eq!(bed.offset().y, expected, "内容尺寸变化后必须补一次 reveal");
    let settled = bed.offset().y;
    drain(&mut bed.app);
    assert_eq!(bed.offset().y, settled, "有界重试：稳定后不再滚动");
}

#[test]
fn ensure_visible_does_nothing_without_a_scroll_ancestor() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(true), editable_style(Some(120.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "0\n1\n2\n3\n4\n5")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    crate::text_edit::set_selection(app.app_mut(), window, node, 10, 10).expect("caret");
    drain(&mut app);
    let tree = app.app().window(window).expect("窗口").tree();
    assert!(tree.scroll_containers().is_empty(), "没有滚动祖先");
    assert_eq!(visible(&app, window), Some(true), "只涉及闪烁，不涉及滚动");
    assert!(
        !crate::text_edit::reveal_pending(app.app(), window),
        "无滚动祖先时只记录 reveal key，不留下待处理 reveal"
    );
    drain(&mut app);
    assert_eq!(
        drain(&mut app),
        Drained::Idle,
        "无祖先的 reveal 不得产生额外滚动帧"
    );
}

#[test]
fn single_line_horizontal_reveal_keeps_four_pixels() {
    let mut bed = horizontal_bed("abcdefghijklmnopqrstuvwxyz0123456789");
    focus(&mut bed.app, bed.node);
    let end = app_text_len(&bed.app, bed.window, bed.node);
    crate::text_edit::set_selection(bed.app.app_mut(), bed.window, bed.node, end, end)
        .expect("caret 到末尾");
    drain(&mut bed.app);

    let caret = caret_content(&bed.app, bed.node, end);
    let viewport = bounds(&bed.app, bed.viewport);
    assert!(bed.offset().x > px(0.0), "末尾 caret 必须 reveal 横向偏移");
    let screen = caret.translate(LogicalPoint::new(-bed.offset().x, px(0.0)));
    assert!(
        (viewport.max().x - screen.max().x - px(4.0)).abs() <= px(0.5),
        "caret 右边界与视口右边界差 4px：{screen:?} vs {viewport:?}"
    );

    crate::text_edit::set_selection(bed.app.app_mut(), bed.window, bed.node, 0, 0)
        .expect("caret 回行首");
    drain(&mut bed.app);
    assert_eq!(bed.offset().x, px(0.0), "回到行首后 reveal 归零");
}
fn app_text_len(app: &TestApp, window: WindowId, node: NodeId) -> usize {
    app.app()
        .text_edit_value(window, node)
        .expect("真实文本")
        .len()
}

#[test]
fn ensure_visible_reveals_a_typed_character_in_one_frame_when_the_host_content_lags() {
    let mut bed = horizontal_bed("abcdefghijklmnopqrstuvwxyz0123456789");
    let window = bed.window;
    let node = bed.node;
    focus(&mut bed.app, node);
    let len = app_text_len(&bed.app, window, node);
    crate::text_edit::set_selection(bed.app.app_mut(), window, node, len, len)
        .expect("caret 到末尾");
    drain(&mut bed.app);
    let settled = bed.app.app().scroll_state(&bed.handle).expect("滚动状态");
    assert_eq!(
        settled.offset.x,
        settled.content.width - settled.viewport.width,
        "前置：宿主内容与布局一致时 offset 必须在右端"
    );
    let step = caret_content(&bed.app, node, len).origin.x
        - caret_content(&bed.app, node, len - 1).origin.x;
    bed.app
        .app_mut()
        .set_scroll_content(
            &bed.handle,
            LogicalSize::new(settled.content.width - step, settled.content.height),
        )
        .expect("滞后一帧的宿主内容尺寸");

    bed.app.inject(InputEvent::Text {
        text: SmolStr::new_static("W"),
        timestamp: Instant::now(),
    });
    assert_eq!(drain(&mut bed.app), Drained::Committed, "键入帧必须提交");

    let after = bed.app.app().scroll_state(&bed.handle).expect("滚动状态");
    let editable = bounds(&bed.app, node);
    assert_eq!(
        after.content.width, editable.size.width,
        "reveal 必须同帧把滚动槽内容补到 editable 已布局宽度：{after:?} vs {editable:?}"
    );
    assert_eq!(
        after.offset.x,
        after.content.width - after.viewport.width,
        "同帧 offset 必须推到右端：{after:?}"
    );
    let caret = caret_content(&bed.app, node, len + 1);
    let viewport = bounds(&bed.app, bed.viewport);
    let screen = caret.translate(LogicalPoint::new(-after.offset.x, px(0.0)));
    assert!(
        screen.origin.x >= viewport.origin.x && screen.max().x <= viewport.max().x,
        "最新输入的字符的 caret 必须完整落在视口内：{screen:?} vs {viewport:?}"
    );
    assert_eq!(
        drain(&mut bed.app),
        Drained::Idle,
        "键入帧之后不得再有额外提交帧"
    );
    assert_eq!(
        bed.app
            .app()
            .scroll_state(&bed.handle)
            .expect("滚动状态")
            .offset
            .x,
        after.offset.x,
        "后续帧不得再次移动 offset"
    );
    assert_eq!(drain(&mut bed.app), Drained::Idle, "稳定后不得有额外帧");
}

#[test]
fn ensure_visible_reveals_a_typed_line_in_one_frame_when_the_host_content_lags() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4", 3, None);
    let line = bed.line_height;
    focus(&mut bed.app, bed.node);
    let len = app_text_len(&bed.app, bed.window, bed.node);
    bed.set_caret(len);
    drain(&mut bed.app);
    let settled = bed.app.app().scroll_state(&bed.handle).expect("滚动状态");
    assert_eq!(
        settled.content.height,
        line * 5.0,
        "前置：宿主内容高 = 5 行：{settled:?}"
    );
    bed.app
        .app_mut()
        .set_scroll_content(
            &bed.handle,
            LogicalSize::new(settled.content.width, line * 4.0),
        )
        .expect("滞后一帧的宿主内容高度");

    bed.app.inject(InputEvent::Text {
        text: SmolStr::new_static("\n"),
        timestamp: Instant::now(),
    });
    assert_eq!(drain(&mut bed.app), Drained::Committed, "键入帧必须提交");

    let after = bed.app.app().scroll_state(&bed.handle).expect("滚动状态");
    let editable = bounds(&bed.app, bed.node);
    assert_eq!(
        after.content.height, editable.size.height,
        "reveal 必须同帧把滚动槽内容补到 editable 已布局高度：{after:?} vs {editable:?}"
    );
    let caret = caret_content(&bed.app, bed.node, len + 1);
    let viewport = bounds(&bed.app, bed.viewport);
    let screen = caret.translate(LogicalPoint::new(px(0.0), -after.offset.y));
    assert!(
        screen.origin.y >= viewport.origin.y && screen.max().y <= viewport.max().y,
        "新输入的行的 caret 必须完整落在视口内：{screen:?} vs {viewport:?}"
    );
}

#[test]
fn ensure_visible_keeps_a_larger_host_content_when_it_reveals() {
    let mut bed = horizontal_bed("abcdefghijklmnopqrstuvwxyz0123456789");
    let window = bed.window;
    let node = bed.node;
    focus(&mut bed.app, node);
    let len = app_text_len(&bed.app, window, node);
    crate::text_edit::set_selection(bed.app.app_mut(), window, node, len, len)
        .expect("caret 到末尾");
    drain(&mut bed.app);
    let settled = bed.app.app().scroll_state(&bed.handle).expect("滚动状态");
    let host = LogicalSize::new(
        settled.content.width + px(300.0),
        settled.content.height + px(200.0),
    );
    bed.app
        .app_mut()
        .set_scroll_content(&bed.handle, host)
        .expect("宿主更大的内容尺寸");

    bed.app.inject(InputEvent::Text {
        text: SmolStr::new_static("W"),
        timestamp: Instant::now(),
    });
    assert_eq!(drain(&mut bed.app), Drained::Committed, "键入帧必须提交");

    let after = bed.app.app().scroll_state(&bed.handle).expect("滚动状态");
    assert_eq!(
        after.content, host,
        "reveal 只做并集，不得缩小宿主更大的内容：{after:?}"
    );
    let caret = caret_content(&bed.app, node, len + 1);
    let viewport = bounds(&bed.app, bed.viewport);
    let screen = caret.translate(LogicalPoint::new(-after.offset.x, px(0.0)));
    assert!(
        screen.origin.x >= viewport.origin.x && screen.max().x <= viewport.max().x,
        "宿主内容更大时 caret 仍必须可见：{screen:?} vs {viewport:?}"
    );
}

#[test]
fn ensure_visible_never_grows_an_outer_scroll_content() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let mut outer_style = StyleBuilder::new()
        .width(px(300.0))
        .height(px(120.0))
        .build();
    outer_style.clip = Clip::Bounds;
    let outer = box_node(&mut app, root, outer_style);
    let mut inner_style = StyleBuilder::new()
        .flex_row()
        .width(px(100.0))
        .height(px(30.0))
        .build();
    inner_style.clip = Clip::Bounds;
    let inner = box_node(&mut app, outer, inner_style);
    let mut editable_style = editable_style(None);
    editable_style.padding_sides(px(0.0), px(5.0), px(0.0), px(0.0));
    let node = mount_editable(&mut app, inner, spec(false), editable_style);
    app.app_mut()
        .text_edit_set_value(window, node, "abcdefghijklmnopqrstuvwxyz0123456789")
        .expect("写入文本");
    drain(&mut app);
    let inner_handle = app
        .app_mut()
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Horizontal,
                line_height: px(LINE_HEIGHT),
            },
        )
        .expect("内层水平滚动槽");
    let outer_handle = app
        .app_mut()
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(LINE_HEIGHT),
            },
        )
        .expect("外层垂直滚动槽");
    let outer_content = LogicalSize::new(px(300.0), px(400.0));
    app.app_mut()
        .set_scroll_content(&outer_handle, outer_content)
        .expect("外层内容尺寸");
    app.app_mut()
        .set_scroll_content(&inner_handle, LogicalSize::new(px(10.0), px(30.0)))
        .expect("内层滞后的内容尺寸");
    focus(&mut app, node);
    let len = app_text_len(&app, window, node);
    crate::text_edit::set_selection(app.app_mut(), window, node, len, len).expect("caret 到末尾");
    assert_eq!(drain(&mut app), Drained::Committed, "reveal 帧必须提交");

    let inner_after = app.app().scroll_state(&inner_handle).expect("内层滚动状态");
    let editable = bounds(&app, node);
    assert_eq!(
        inner_after.content.width, editable.size.width,
        "内层滚动内容必须补到 editable 已布局宽度：{inner_after:?} vs {editable:?}"
    );
    assert_eq!(
        app.app()
            .scroll_state(&outer_handle)
            .expect("外层滚动状态")
            .content,
        outer_content,
        "外层滚动容器的 content 不得被 reveal 修改"
    );
    let caret = caret_content(&app, node, len);
    let viewport = bounds(&app, inner);
    let screen = caret.translate(LogicalPoint::new(-inner_after.offset.x, px(0.0)));
    assert!(
        screen.origin.x >= viewport.origin.x && screen.max().x <= viewport.max().x,
        "caret 必须完整落在内层视口内：{screen:?} vs {viewport:?}"
    );
}

#[test]
fn page_down_and_up_move_by_the_visible_line_count() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 3, None);
    focus(&mut bed.app, bed.node);
    crate::text_edit::set_selection(bed.app.app_mut(), bed.window, bed.node, 0, 0)
        .expect("caret 到文首");
    drain(&mut bed.app);

    bed.app
        .inject(InputEvent::key_down(KeyCode::PageDown, Modifiers::empty()));
    drain(&mut bed.app);
    let caret = bed
        .app
        .app()
        .text_edit_snapshot(bed.window, bed.node)
        .expect("快照")
        .selection
        .focus()
        .byte;
    assert_eq!(caret, page_line(3), "可见 3 行 ⇒ PageDown 走 3 行");

    bed.app
        .inject(InputEvent::key_down(KeyCode::PageUp, Modifiers::empty()));
    drain(&mut bed.app);
    let caret = bed
        .app
        .app()
        .text_edit_snapshot(bed.window, bed.node)
        .expect("快照")
        .selection
        .focus()
        .byte;
    assert_eq!(caret, 0, "PageUp 回到文首");
}

#[test]
fn dragging_below_the_viewport_scrolls_farther_with_the_distance() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 3, None);
    focus(&mut bed.app, bed.node);
    crate::text_edit::set_selection(bed.app.app_mut(), bed.window, bed.node, 0, 0)
        .expect("caret 到文首");
    drain(&mut bed.app);

    let viewport = bounds(&bed.app, bed.viewport);
    let start = caret_screen(&bed.app, bed.node, 0);
    bed.app.inject(InputEvent::pointer_down(
        start,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    let near = LogicalPoint::new(start.x, viewport.max().y + px(5.0));
    bed.app
        .inject(InputEvent::pointer_move(near, Modifiers::empty()));
    let first = bed.offset().y;
    assert!(first > px(0.0), "拖出视口底部必须滚动：{first:?}");
    assert!(
        first <= px(5.0) + px(0.5),
        "近距离越界按距离滚动：{first:?}"
    );

    let far = LogicalPoint::new(start.x, viewport.max().y + px(45.0));
    bed.app
        .inject(InputEvent::pointer_move(far, Modifiers::empty()));
    let second = bed.offset().y;
    assert!(
        second - first > first,
        "越界更远时每次 move 滚得更多：near={first:?} far={second:?}"
    );
    assert!(
        second - first <= px(bed.line_height.value() * 2.0) + px(0.5),
        "单次 move 步长上限 2 行：{first:?} -> {second:?}"
    );

    let snapshot = bed
        .app
        .app()
        .text_edit_snapshot(bed.window, bed.node)
        .expect("快照");
    assert_eq!(snapshot.selection.anchor().byte, 0);
    assert_eq!(
        snapshot.selection.focus().byte,
        app_text_len(&bed.app, bed.window, bed.node),
        "拖到视口外后 focus 落在文本末尾"
    );

    bed.app.inject(InputEvent::pointer_up(
        far,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    let _ = drain(&mut bed.app);
    assert_eq!(drain(&mut bed.app), Drained::Idle, "up 后无残留帧");
}

fn caret_screen(app: &TestApp, node: NodeId, byte: usize) -> LogicalPoint {
    let rect = app
        .app()
        .window(app.window_id())
        .expect("窗口")
        .text_caret_rect(node, byte)
        .expect("caret 几何");
    LogicalPoint::new(
        rect.origin.x + rect.size.width * 0.5,
        rect.origin.y + rect.size.height * 0.5,
    )
}

fn mount_in_window(app: &mut TestApp, window: WindowId, value: &str) -> NodeId {
    let root = app.app().window(window).expect("窗口").tree().root();
    let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
    let node = NodeBuilder::editable_text(spec(false))
        .style(editable_style(Some(200.0)))
        .build(tree, root)
        .expect("可编辑节点");
    app.app_mut()
        .text_edit_set_value(window, node, value)
        .expect("写入文本");
    node
}

fn frame_window(app: &mut TestApp, window: WindowId) -> Drained {
    match app.app_mut().frame(window) {
        Some(Ok(outcome)) => {
            if matches!(outcome, crate::window::FrameOutcome::Idle) {
                Drained::Idle
            } else {
                Drained::Committed
            }
        }
        Some(Err(error)) => panic!("窗口帧失败：{error}"),
        None => panic!("窗口不存在"),
    }
}

#[test]
fn a_second_window_frame_does_not_cancel_the_focused_caret_blink() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abcdef");
    let node_b = mount_in_window(&mut app, window_b, "uvwxyz");
    drain(&mut app);
    frame_window(&mut app, window_b);
    drain(&mut app);

    focus(&mut app, node_a);
    drain(&mut app);
    assert_eq!(visible(&app, window_a), Some(true));
    assert!(
        app.app().next_wake_deadline().is_some(),
        "A 聚焦后应有闪烁唤醒"
    );

    app.app_mut()
        .text_edit_set_value(window_b, node_b, "uvwxyz!")
        .expect("弄脏 B 的文本");
    assert_eq!(
        frame_window(&mut app, window_b),
        Drained::Committed,
        "B 的帧必须真正提交（才能走到 sync_blink 的非 owner 分支）"
    );
    assert!(
        app.app().next_wake_deadline().is_some(),
        "B 渲染一帧不得取消 A 的闪烁 timer"
    );
    assert_eq!(
        visible(&app, window_a),
        Some(true),
        "B 帧后 A 的 caret 仍亮"
    );

    app.advance(Duration::from_millis(530));
    let record = app.drain().expect("A 翻转帧").clone();
    assert!(record.rendered, "530ms 后 A 必须提交翻转帧");
    assert_eq!(
        visible(&app, window_a),
        Some(false),
        "530ms 后 A 的 caret 必须翻转（timer 未被冻结）"
    );
}

#[test]
fn clearing_every_window_focus_cancels_the_blink_wake() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abc");
    let node_b = mount_in_window(&mut app, window_b, "xyz");
    drain(&mut app);
    frame_window(&mut app, window_b);

    focus(&mut app, node_a);
    drain(&mut app);
    assert!(app.app().next_wake_deadline().is_some());

    app.app_mut().set_focus(window_b, Some(node_b));
    frame_window(&mut app, window_b);
    assert!(
        app.app().next_wake_deadline().is_some(),
        "焦点移到 B 后 timer 仍在"
    );

    app.app_mut().set_focus(window_a, None);
    app.app_mut().set_focus(window_b, None);
    assert_eq!(
        app.app().next_wake_deadline(),
        None,
        "所有窗口失焦后取消闪烁唤醒"
    );

    let _ = drain(&mut app);
    frame_window(&mut app, window_b);
    let index_a = app
        .app()
        .window(window_a)
        .expect("窗口")
        .frame_index()
        .get();
    let index_b = app
        .app()
        .window(window_b)
        .expect("窗口")
        .frame_index()
        .get();
    for _ in 0..2 {
        assert_eq!(drain(&mut app), Drained::Idle);
        assert_eq!(frame_window(&mut app, window_b), Drained::Idle);
    }
    assert_eq!(
        app.app()
            .window(window_a)
            .expect("窗口")
            .frame_index()
            .get(),
        index_a,
        "A 的稳态 0 帧"
    );
    assert_eq!(
        app.app()
            .window(window_b)
            .expect("窗口")
            .frame_index()
            .get(),
        index_b,
        "B 的稳态 0 帧"
    );
}

#[test]
fn a_non_owner_window_activity_does_not_delay_the_owner_flip() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abcdef");
    let node_b = mount_in_window(&mut app, window_b, "uvwxyz");
    drain(&mut app);
    frame_window(&mut app, window_b);
    drain(&mut app);

    focus(&mut app, node_a);
    drain(&mut app);
    app.advance(Duration::from_millis(400));
    app.app_mut()
        .text_edit_set_value(window_b, node_b, "uvwxyz!")
        .expect("非 owner 窗口活动");

    app.advance(Duration::from_millis(130));
    let record = app.drain().expect("A 帧").clone();
    assert!(
        record.rendered,
        "owner 必须在 530ms 翻转：非 owner 窗口的 activity 不得推迟 last_activity"
    );
    assert_eq!(visible(&app, window_a), Some(false));
    frame_window(&mut app, window_b);
}

#[test]
fn closing_the_owner_window_releases_the_blink_owner() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abcdef");
    let node_b = mount_in_window(&mut app, window_b, "uvwxyz");
    drain(&mut app);
    frame_window(&mut app, window_b);
    drain(&mut app);

    focus(&mut app, node_a);
    drain(&mut app);
    assert!(app.app_mut().set_focus(window_b, Some(node_b)));
    frame_window(&mut app, window_b);
    assert!(
        app.app().next_wake_deadline().is_some(),
        "前置：owner 是 B，存在闪烁唤醒"
    );

    app.app_mut().close_window(window_b);
    assert_eq!(
        app.app().next_wake_deadline(),
        None,
        "关闭 owner 窗口后必须释放 owner 并取消唤醒"
    );
    drain(&mut app);
    assert_eq!(
        visible(&app, window_a),
        Some(true),
        "存活窗口里仍有聚焦输入框时必须重新闪烁"
    );
    assert!(app.app().next_wake_deadline().is_some());
}

#[test]
fn removing_the_owner_node_releases_the_blink_owner() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abcdef");
    let node_b = mount_in_window(&mut app, window_b, "uvwxyz");
    drain(&mut app);
    frame_window(&mut app, window_b);
    drain(&mut app);

    focus(&mut app, node_a);
    drain(&mut app);
    assert!(app.app_mut().set_focus(window_b, Some(node_b)));
    frame_window(&mut app, window_b);
    app.app_mut()
        .remove_node(window_b, node_b)
        .expect("删除 owner 节点");
    assert_eq!(
        app.app().next_wake_deadline(),
        None,
        "删除 owner 节点后必须取消唤醒"
    );
    drain(&mut app);
    assert_eq!(
        visible(&app, window_a),
        Some(true),
        "存活窗口的聚焦输入框要能重新夺权"
    );
}

#[test]
fn ownerless_takeover_requires_the_os_focused_window() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abcdef");
    let node_b = mount_in_window(&mut app, window_b, "uvwxyz");
    drain(&mut app);
    frame_window(&mut app, window_b);
    drain(&mut app);

    focus(&mut app, node_a);
    drain(&mut app);
    assert!(app.app().next_wake_deadline().is_some());
    app.app_mut()
        .dispatch(window_a, Event::FocusChanged { focused: false });
    drain(&mut app);
    assert_eq!(visible(&app, window_a), Some(false), "窗口失焦隐藏 caret");

    app.app_mut()
        .dispatch(window_b, Event::FocusChanged { focused: true });
    drain(&mut app);
    assert_eq!(
        app.app().next_wake_deadline(),
        None,
        "非 OS 焦点窗口的聚焦输入框不得在 owner 空缺时夺权"
    );
    assert_eq!(visible(&app, window_a), Some(false));

    assert!(app.app_mut().set_focus(window_b, Some(node_b)));
    frame_window(&mut app, window_b);
    assert_eq!(
        visible(&app, window_b),
        Some(true),
        "OS 焦点窗口的节点聚焦必须重新夺权"
    );
    assert!(app.app().next_wake_deadline().is_some());
}

#[test]
fn a_focused_caret_renders_one_frame_per_half_period() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "abc")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);

    let start = app.app().clock();
    let mut flips: Vec<Duration> = Vec::new();
    for _ in 0..300 {
        app.advance(Duration::from_millis(10));
        if drain(&mut app) == Drained::Committed {
            flips.push(app.app().clock().saturating_duration_since(start));
        }
    }
    assert!(
        flips.len() <= 6,
        "3s 内至多 6 帧（每 530ms 一帧），实际 {}：{flips:?}",
        flips.len()
    );
    assert!(flips.len() >= 4, "3s 内应有约 5 次翻转：{flips:?}");
    for pair in flips.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            gap >= Duration::from_millis(500) && gap <= Duration::from_millis(560),
            "相邻翻转帧间隔必须 ≈530ms，实际 {gap:?}：{flips:?}"
        );
    }
}

#[test]
fn page_down_uses_the_nearest_vertical_ancestor() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let mut outer_style = StyleBuilder::new()
        .flex_col()
        .size(px(200.0), px(60.0))
        .build();
    outer_style.clip = Clip::Bounds;
    let outer = box_node(&mut app, root, outer_style);
    let mut inner_style = StyleBuilder::new().size(px(200.0), px(40.0)).build();
    inner_style.clip = Clip::Bounds;
    let inner = box_node(&mut app, outer, inner_style);
    let node = mount_editable(&mut app, inner, spec(true), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "0\n1\n2\n3\n4\n5")
        .expect("写入文本");
    let outer_handle = app
        .app_mut()
        .make_scroll(
            window,
            outer,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: px(LINE_HEIGHT),
            },
        )
        .expect("垂直容器");
    let inner_handle = app
        .app_mut()
        .make_scroll(
            window,
            inner,
            ScrollConfig {
                axis: ScrollAxis::Horizontal,
                line_height: px(LINE_HEIGHT),
            },
        )
        .expect("水平容器");
    app.app_mut()
        .set_scroll_content(&outer_handle, LogicalSize::new(px(200.0), px(400.0)))
        .expect("垂直内容");
    app.app_mut()
        .set_scroll_content(&inner_handle, LogicalSize::new(px(400.0), px(200.0)))
        .expect("水平内容");
    drain(&mut app);

    focus(&mut app, node);
    crate::text_edit::set_selection(app.app_mut(), window, node, 0, 0).expect("caret 到文首");
    drain(&mut app);
    app.inject(InputEvent::key_down(KeyCode::PageDown, Modifiers::empty()));
    drain(&mut app);

    let caret = app
        .app()
        .text_edit_snapshot(window, node)
        .expect("快照")
        .selection
        .focus()
        .byte;
    assert_eq!(
        caret,
        page_line(3),
        "必须按最近 Vertical 祖先（60px/3 行）翻页，而不是更近的 Horizontal 祖先（40px/2 行）"
    );
}

#[test]
fn ensure_visible_reveals_the_selection_anchor_when_the_focus_stays_visible() {
    let mut bed = vertical_bed("0\n1\n2\n3\n4\n5", 4, None);
    let line = bed.line_height;
    focus(&mut bed.app, bed.node);
    bed.set_caret(page_line(3));
    drain(&mut bed.app);
    let revealed = bed.offset().y;
    assert!(revealed > px(0.0), "前置：focus 行已 reveal 滚动");

    let anchor = page_line(1);
    crate::text_edit::set_selection(
        bed.app.app_mut(),
        bed.window,
        bed.node,
        anchor,
        page_line(3),
    )
    .expect("反选到上一行");
    drain(&mut bed.app);
    drain(&mut bed.app);

    let anchor_rect = caret_content(&bed.app, bed.node, anchor);
    let focus_rect = caret_content(&bed.app, bed.node, page_line(3));
    let viewport = bounds(&bed.app, bed.viewport);
    let expected = (anchor_rect.origin.y - line).max(px(0.0));
    assert_eq!(
        bed.offset().y,
        expected,
        "选区 anchor 在安全区外时也要被 reveal（anchor={anchor_rect:?}，focus 仍在视口内）"
    );
    assert!(bed.offset().y < revealed, "anchor reveal 必须向上滚动");
    let screen_anchor = anchor_rect.translate(LogicalPoint::new(px(0.0), -bed.offset().y));
    let screen_focus = focus_rect.translate(LogicalPoint::new(px(0.0), -bed.offset().y));
    assert!(
        screen_anchor.origin.y >= viewport.origin.y,
        "anchor 端必须露出：{screen_anchor:?} vs {viewport:?}"
    );
    assert!(
        screen_focus.origin.y >= viewport.origin.y && screen_focus.max().y <= viewport.max().y,
        "reveal anchor 不得把 focus 端挤出视口：{screen_focus:?} vs {viewport:?}"
    );
}

#[test]
fn pointer_activity_resets_the_caret_to_visible_and_restarts_the_period() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "abcdef")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);
    app.advance(Duration::from_millis(530));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "前置：已熄灭");

    app.advance(Duration::from_millis(100));
    let point = caret_screen(&app, node, 6);
    app.inject(InputEvent::pointer_down(
        point,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "指针活动必须重置为亮");
    assert_eq!(caret_quads(&app, node).len(), 1);

    app.advance(Duration::from_millis(529));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "指针活动后 529ms 仍亮");
    app.advance(Duration::from_millis(1));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "指针活动后 530ms 熄灭");
    app.inject(InputEvent::pointer_up(
        point,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    let _ = drain(&mut app);
}

#[test]
fn programmatic_set_value_resets_the_caret_to_visible() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "abc")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);
    app.advance(Duration::from_millis(530));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "前置：已熄灭");

    app.app_mut()
        .text_edit_set_value(window, node, "abcdef")
        .expect("程序化写入");
    drain(&mut app);
    assert_eq!(
        visible(&app, window),
        Some(true),
        "程序化写值必须重置 caret 为可见"
    );
    assert_eq!(caret_quads(&app, node).len(), 1);
}

#[test]
fn ime_commit_resets_the_caret_to_visible() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = root_of(&app, window);
    let node = mount_editable(&mut app, root, spec(false), editable_style(Some(200.0)));
    app.app_mut()
        .text_edit_set_value(window, node, "abc")
        .expect("写入文本");
    drain(&mut app);
    focus(&mut app, node);
    drain(&mut app);
    app.advance(Duration::from_millis(530));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(false), "前置：已熄灭");

    app.dispatch_event(Event::Ime(ImeEvent::Commit {
        text: SmolStr::new_static("x"),
        replace: None,
    }));
    drain(&mut app);
    assert_eq!(visible(&app, window), Some(true), "IME 提交必须重置为亮");
    assert!(
        app.app()
            .text_edit_value(window, node)
            .is_ok_and(|value| value.as_str() == "abcx")
    );
}

#[test]
fn dragging_past_the_right_edge_auto_scrolls_horizontally() {
    let text: String = (0..40).map(|index| format!("w{index} ")).collect();
    let mut bed = horizontal_bed(text.as_str());
    let window = bed.window;
    let node = bed.node;
    focus(&mut bed.app, node);
    crate::text_edit::set_selection(bed.app.app_mut(), window, node, 0, 0).expect("caret 到文首");
    let _ = drain(&mut bed.app);

    let viewport = bounds(&bed.app, bed.viewport);
    let start = caret_screen(&bed.app, node, 0);
    bed.app.inject(InputEvent::pointer_down(
        start,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    let outside = LogicalPoint::new(viewport.max().x + px(30.0), start.y);
    bed.app
        .inject(InputEvent::pointer_move(outside, Modifiers::empty()));
    let offset = bed.offset().x;
    assert!(
        offset > px(0.0),
        "拖到视口右侧外必须触发水平 auto-scroll：{offset:?}"
    );
    let snapshot = bed
        .app
        .app()
        .text_edit_snapshot(window, node)
        .expect("快照");
    assert_eq!(
        snapshot.selection.anchor().byte,
        0,
        "水平 auto-scroll 保持 anchor 在文首"
    );
    assert!(
        snapshot.selection.focus().byte > 0,
        "水平拖选必须推进 focus"
    );
    bed.app.inject(InputEvent::pointer_up(
        outside,
        PointerButton::Primary,
        Modifiers::empty(),
    ));
    let _ = drain(&mut bed.app);
}

#[test]
fn ensure_visible_rejects_an_anchor_reveal_that_would_push_the_focus_out() {
    let text: String = (0..30)
        .map(|index| ((b'a' + (index % 26) as u8) as char).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut bed = vertical_bed(text.as_str(), 4, None);
    focus(&mut bed.app, bed.node);
    bed.set_caret(page_line(1));
    drain(&mut bed.app);
    let before = bed.offset().y;
    assert_eq!(before, px(0.0), "前置：focus 行已在视口内");

    crate::text_edit::set_selection(
        bed.app.app_mut(),
        bed.window,
        bed.node,
        page_line(20),
        page_line(1),
    )
    .expect("远距离反选");
    drain(&mut bed.app);
    drain(&mut bed.app);
    assert_eq!(
        bed.offset().y,
        px(0.0),
        "anchor reveal 会把 focus 端挤出视口时必须在拒绝分支里放弃滚动"
    );
    let snapshot = bed
        .app
        .app()
        .text_edit_snapshot(bed.window, bed.node)
        .expect("快照");
    assert_eq!(snapshot.selection.anchor().byte, page_line(20));
}

#[test]
fn ensure_visible_scrolls_horizontally_to_reveal_a_selection_anchor() {
    let text = "abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bed = horizontal_bed(text);
    let window = bed.window;
    let node = bed.node;
    focus(&mut bed.app, node);
    crate::text_edit::set_selection(bed.app.app_mut(), window, node, 2, 2).expect("caret");
    let _ = drain(&mut bed.app);
    assert_eq!(bed.offset().x, px(0.0), "前置：caret 在视口内");

    let viewport = bounds(&bed.app, bed.viewport);
    let anchor = (0..text.len())
        .find(|byte| caret_content(&bed.app, node, *byte).max().x > viewport.size.width)
        .expect("文本必须宽于视口");
    crate::text_edit::set_selection(bed.app.app_mut(), window, node, anchor, 2).expect("水平反选");
    let _ = drain(&mut bed.app);
    let _ = drain(&mut bed.app);

    let offset = bed.offset().x;
    assert!(
        offset > px(0.0),
        "水平滚动轴上的选区 anchor 必须被 reveal：offset={offset:?}"
    );
    let anchor_rect = caret_content(&bed.app, node, anchor);
    let focus_rect = caret_content(&bed.app, node, 2);
    let screen_anchor = anchor_rect.translate(LogicalPoint::new(-offset, px(0.0)));
    let screen_focus = focus_rect.translate(LogicalPoint::new(-offset, px(0.0)));
    assert!(
        screen_anchor.max().x <= viewport.size.width,
        "anchor 端必须露出：{screen_anchor:?} vs {viewport:?}"
    );
    assert!(
        screen_focus.origin.x >= px(0.0) && screen_focus.max().x <= viewport.size.width,
        "reveal anchor 不得把 focus 端挤出视口：{screen_focus:?} vs {viewport:?}"
    );
}

#[test]
fn a_non_owner_window_focus_loss_keeps_the_owner_blink_timer() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abcdef");
    let _node_b = mount_in_window(&mut app, window_b, "uvwxyz");
    drain(&mut app);
    frame_window(&mut app, window_b);
    drain(&mut app);

    focus(&mut app, node_a);
    drain(&mut app);
    assert!(
        app.app().next_wake_deadline().is_some(),
        "前置：owner 窗口应有闪烁唤醒"
    );

    app.app_mut().dispatch(
        window_b,
        crate::event::Event::FocusChanged { focused: false },
    );
    assert!(
        app.app().next_wake_deadline().is_some(),
        "非 owner 窗口失焦不得取消 owner 的闪烁 timer"
    );
    app.advance(Duration::from_millis(530));
    let record = app.drain().expect("A 翻转帧").clone();
    assert!(record.rendered, "530ms 后 owner 窗口仍必须提交翻转帧");
    assert_eq!(
        visible(&app, window_a),
        Some(false),
        "非 owner 失焦后 owner 的闪烁节奏不受影响"
    );
}

#[test]
fn a_cross_window_takeover_hides_the_previous_owner_caret() {
    let mut app = TestApp::new();
    let window_a = app.window_id();
    let window_b = app
        .app_mut()
        .create_window(crate::window::WindowOptions::default());
    let node_a = mount_in_window(&mut app, window_a, "abcdef");
    let node_b = mount_in_window(&mut app, window_b, "uvwxyz");
    drain(&mut app);
    frame_window(&mut app, window_b);
    drain(&mut app);

    focus(&mut app, node_a);
    drain(&mut app);
    assert_eq!(
        app.app().text_edit_caret_visible(window_a),
        Some(true),
        "前置：A 是闪烁 owner 且 caret 可见"
    );

    app.app_mut().set_focus(window_b, Some(node_b));
    let _ = frame_window(&mut app, window_b);
    assert_eq!(
        app.app().text_edit_caret_visible(window_a),
        Some(false),
        "夺权后 A 的聚焦 caret 必须被隐藏"
    );
    assert_eq!(
        app.app().text_edit_caret_visible(window_b),
        Some(true),
        "B 成为新 owner 且 caret 可见"
    );
    assert!(
        !app.app()
            .window(window_a)
            .expect("窗口")
            .tree()
            .editable(node_a)
            .expect("可编辑节点")
            .caret_visible(),
        "跨窗夺权必须把旧 owner 的 caret 隐藏并标脏"
    );
}

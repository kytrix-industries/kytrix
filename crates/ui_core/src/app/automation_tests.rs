//! E25 / P6-1：`automation` feature 的引擎级用例。
//!
//! `automation` 是**生产可见**的注入 / 观测面（kytrix 的 `--acceptance` 真实窗口自检要它），
//! 所以这里不依赖 `test-support`：断言面全部是公开 API（`App::frame` / `Window::frame_index` /
//! `Window::scene`）+ 本 feature 自己的四个读点。
//!
//! 钉住的契约（spec-delta E25）：
//! - 注入走与平台泵**同一条** `dispatch_input`（命中 / 动作 / 回执语义一致）；
//! - 三个读取面**不请求帧**（I3），未知窗口读作空 / `None`；
//! - `painted_primitives` 是场景图元数（最近一帧画了什么），`texts` 是树序文本载荷。

use std::{cell::Cell, rc::Rc};

use super::App;
use crate::{
    event::{Action, InputEvent, Modifiers, PointerButton},
    math::{Point, Rect, Scale, Size},
    node::builder::NodeBuilder,
    paint::{
        color::Color,
        style::{PaintSpec, StylePatch},
    },
    window::{FrameOutcome, WindowId, WindowOptions},
};

fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: "automation".into(),
        size: Size::new(400.0, 300.0),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    (runtime, app, window)
}

fn frame(app: &mut App, window: WindowId) -> FrameOutcome {
    app.frame(window)
        .expect("window exists")
        .expect("frame succeeds")
}

fn background() -> PaintSpec {
    PaintSpec {
        background: Color::from_rgba8([200, 40, 40, 255]),
        ..PaintSpec::default()
    }
}

/// Injected input takes the platform path: the hit test, the action dispatch and the
/// receipt are the ones a real pointer produces.
#[test]
fn injected_input_should_take_the_platform_path() {
    let (_runtime, mut app, window) = setup();
    let calls = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&calls);
    app.on_action(Action::custom("automation.hit"), move |_ctx| {
        seen.set(seen.get() + 1);
    });
    let root = app.window(window).expect("window").tree().root();
    let node = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 80.0, 40.0))
        .paint(background())
        .on_click(Action::custom("automation.hit"))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_style_patch_for_test(
            node,
            StylePatch {
                interactive: Some(true),
                ..StylePatch::default()
            },
        );
    frame(&mut app, window);
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .clear_all_dirty_for_test();

    let down = app.inject_input(
        window,
        InputEvent::pointer_down(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.inject_input(
        window,
        InputEvent::pointer_up(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );

    assert_eq!(
        down.hit.map(|hit| hit.node),
        Some(node),
        "the press hits the node the pointer is over"
    );
    assert_eq!(
        up.action,
        Some(Action::custom("automation.hit")),
        "the release dispatches the node's action"
    );
    assert_eq!(up.target, Some(node));
    assert_eq!(up.click_count, 1);
    assert_eq!(calls.get(), 1, "the handler ran exactly once");
}

/// Injection is the only write: the three read points answer from state the engine already
/// has, and none of them asks for a frame (I3).
#[test]
fn reading_the_automation_surface_should_not_request_frames() {
    let (_runtime, mut app, window) = setup();
    root_text(&mut app, window, "hello");
    frame(&mut app, window);
    let requests = app.frame_requests();

    let index = app
        .frame_index(window)
        .expect("the window is alive after a commit");
    let primitives = app.painted_primitives(window);
    let texts = app.texts(window);

    assert_eq!(
        app.frame_requests(),
        requests,
        "frame_index / painted_primitives / texts are reads"
    );
    assert!(index.get() >= 1, "a commit was observed");
    assert!(!texts.is_empty(), "the window draws text");
    let _ = primitives;
}

/// The frame cursor is the window's committed cursor, and a second commit advances it.
#[test]
fn the_frame_cursor_should_be_the_committed_one() {
    let (_runtime, mut app, window) = setup();
    root_text(&mut app, window, "hello");
    frame(&mut app, window);
    let first = app.frame_index(window).expect("alive").get();

    app.request_repaint(window).expect("the window is alive");
    frame(&mut app, window);
    let second = app.frame_index(window).expect("alive").get();

    assert!(second > first, "a committed repaint advances the cursor");
    assert_eq!(
        Some(second),
        app.window(window).map(|window| window.frame_index().get())
    );
}

/// The painted-primitive count is the scene's primitive count: empty before anything is
/// painted, positive after a painted node commits.
#[test]
fn the_painted_primitive_count_should_follow_the_scene() {
    let (_runtime, mut app, window) = setup();
    assert_eq!(app.painted_primitives(window), 0, "nothing painted yet");

    let root = app.window(window).expect("window").tree().root();
    NodeBuilder::boxed()
        .layout_override(Rect::new(4.0, 4.0, 60.0, 20.0))
        .paint(background())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
    frame(&mut app, window);

    let painted = app.painted_primitives(window);
    let scene = app
        .window(window)
        .expect("window")
        .scene()
        .primitive_count();
    assert!(painted > 0, "the painted box is in the scene");
    assert_eq!(painted, scene, "the read is the scene's count");
}

/// The text read is the tree's payloads in tree order — what the frame draws, not what the
/// application asked for.
#[test]
fn the_text_read_should_report_the_tree_in_order() {
    let (_runtime, mut app, window) = setup();
    let root = app.window(window).expect("window").tree().root();
    for text in ["first", "second", "third"] {
        NodeBuilder::text(text, crate::text::TextStyle::new(12.0))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
    }

    assert_eq!(
        app.texts(window),
        ["first", "second", "third"]
            .map(smol_str::SmolStr::new_static)
            .to_vec(),
        "depth-first, pre-order: the order the tree paints in"
    );
}

/// A closed window has no cursor, no scene and no texts; an injection sent to it is a no-op
/// receipt instead of a panic or a stale write.
#[test]
fn an_unknown_window_should_read_as_empty_and_ignore_injection() {
    let (_runtime, mut app, window) = setup();
    root_text(&mut app, window, "hello");
    frame(&mut app, window);
    assert!(app.close_window(window), "the window closes");

    assert_eq!(app.frame_index(window), None);
    assert_eq!(app.painted_primitives(window), 0);
    assert!(app.texts(window).is_empty());
    let outcome = app.inject_input(
        window,
        InputEvent::pointer_move(Point::new(1.0, 2.0), Modifiers::empty()),
    );
    assert_eq!(outcome, crate::event::InputOutcome::default());
}

/// One text node at the root, for the tests that only need a non-empty tree.
fn root_text(app: &mut App, window: WindowId, text: &'static str) {
    let root = app.window(window).expect("window").tree().root();
    NodeBuilder::text(text, crate::text::TextStyle::new(12.0))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
}

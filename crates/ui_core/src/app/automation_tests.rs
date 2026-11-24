use std::{cell::Cell, rc::Rc};

use super::App;
use crate::{
    event::{Action, InputEvent, Modifiers, PointerButton},
    math::{LogicalPoint, LogicalRect, LogicalSize, px},
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
        size: LogicalSize::new(px(400.0), px(300.0)),
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
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(80.0), px(40.0)))
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
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let up = app.inject_input(
        window,
        InputEvent::pointer_up(
            LogicalPoint::new(px(20.0), px(20.0)),
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

#[test]
fn the_painted_primitive_count_should_follow_the_scene() {
    let (_runtime, mut app, window) = setup();
    assert_eq!(app.painted_primitives(window), 0, "nothing painted yet");

    let root = app.window(window).expect("window").tree().root();
    NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(4.0), px(4.0), px(60.0), px(20.0)))
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

#[test]
fn the_text_read_should_report_the_tree_in_order() {
    let (_runtime, mut app, window) = setup();
    let root = app.window(window).expect("window").tree().root();
    for text in ["first", "second", "third"] {
        NodeBuilder::text(text, crate::text::TextStyle::new(px(12.0)))
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
        InputEvent::pointer_move(LogicalPoint::new(px(1.0), px(2.0)), Modifiers::empty()),
    );
    assert_eq!(outcome, crate::event::InputOutcome::default());
}

fn root_text(app: &mut App, window: WindowId, text: &'static str) {
    let root = app.window(window).expect("window").tree().root();
    NodeBuilder::text(text, crate::text::TextStyle::new(px(12.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount");
}

use std::{cell::Cell, rc::Rc, time::Duration};

use super::App;
use crate::{
    error::EngineError,
    event::{Action, InputEvent, KeyBinding, KeyCode, Modifiers},
    interaction::{AnimatedValue, AnimationSpec, Easing, ScrollAxis, ScrollConfig},
    math::{Scale, Size},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, Transient},
    },
    paint::{color::Color, style::Clip, style_builder::StyleBuilder},
    window::{FrameOutcome, WindowId, WindowOptions},
};

fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    let window = app.create_window(WindowOptions {
        title: "e8".into(),
        size: Size::new(400.0, 300.0),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    (rt, app, window)
}

fn root(app: &App, window: WindowId) -> NodeId {
    app.window(window).expect("window").tree().root()
}

fn mount_box(app: &mut App, window: WindowId, parent: NodeId) -> NodeId {
    let style = StyleBuilder::new().size(40.0, 40.0).build();
    NodeBuilder::boxed()
        .style(style)
        .on_click(Action::custom("focusable"))
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("node")
}

fn mount_scope(app: &mut App, window: WindowId, parent: NodeId) -> NodeId {
    mount_box(app, window, parent)
}

fn mount_scroll_container(app: &mut App, window: WindowId, parent: NodeId) -> NodeId {
    let mut style = StyleBuilder::new().size(200.0, 150.0).build();
    style.clip = Clip::Bounds;
    NodeBuilder::boxed()
        .style(style)
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("scroll container")
}

fn settle(app: &mut App, window: WindowId) {
    for _ in 0..8 {
        let outcome = app.frame(window).expect("window").expect("frame");
        if matches!(outcome, FrameOutcome::Idle) {
            return;
        }
    }
    panic!("帧管线在 8 帧内没有回到 Idle");
}

fn binding(key: KeyCode, action: Action) -> KeyBinding {
    KeyBinding {
        key,
        modifiers: Modifiers::empty(),
        action,
    }
}

fn press(app: &mut App, window: WindowId, code: KeyCode) {
    app.dispatch_input(window, InputEvent::key_down(code, Modifiers::empty()));
}

fn scoped_bindings(app: &App) -> usize {
    app.scoped_bindings.len()
}

fn set_focus(app: &mut App, window: WindowId, node: NodeId) {
    assert!(app.set_focus(window, Some(node)), "节点必须可聚焦");
}

fn hit_counter(app: &mut App, action: &Action) -> Rc<Cell<u32>> {
    let hits = Rc::new(Cell::new(0));
    let counter = hits.clone();
    app.on_action(action.clone(), move |_ctx| {
        counter.set(counter.get() + 1);
    });
    hits
}

#[test]
fn a_scoped_binding_consumes_only_while_the_focus_is_in_its_subtree() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = mount_scope(&mut app, window, root);
    let inside = mount_box(&mut app, window, scope);
    let outside = mount_box(&mut app, window, root);

    let scoped = Action::custom("scoped");
    let global = Action::custom("global");
    let scoped_hits = hit_counter(&mut app, &scoped);
    let global_hits = hit_counter(&mut app, &global);
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, scoped.clone()))
        .expect("scope 节点存在");
    app.bind_key(binding(KeyCode::ArrowDown, global.clone()));

    set_focus(&mut app, window, inside);
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(scoped_hits.get(), 1);
    assert_eq!(global_hits.get(), 0, "scoped 命中后不再走全局绑定");

    set_focus(&mut app, window, outside);
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(scoped_hits.get(), 1, "焦点不在子树内不得触发");
    assert_eq!(global_hits.get(), 1, "必须回落到全局绑定");

    assert!(app.set_focus(window, None));
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(scoped_hits.get(), 1);
    assert_eq!(global_hits.get(), 2, "全局绑定不依赖焦点");
}

#[test]
fn a_scoped_binding_without_a_handler_does_not_swallow_the_key() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = mount_scope(&mut app, window, root);
    let inside = mount_box(&mut app, window, scope);
    set_focus(&mut app, window, inside);

    let action = Action::custom("no-handler");
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, action))
        .expect("bind");
    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty()),
    );
    assert_eq!(outcome.action, None, "没有 handler 的命中不消费按键");

    let global = Action::custom("global");
    let hits = hit_counter(&mut app, &global);
    app.bind_key(binding(KeyCode::ArrowDown, global.clone()));
    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty()),
    );
    assert_eq!(hits.get(), 1);
    assert_eq!(outcome.action, Some(global));
}

#[test]
fn a_scoped_binding_without_a_handler_falls_back_to_default_key_scroll() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = mount_scroll_container(&mut app, window, root);
    let scope = mount_scope(&mut app, window, container);
    let inside = mount_box(&mut app, window, scope);
    settle(&mut app, window);
    let handle = app
        .make_scroll(
            window,
            container,
            ScrollConfig {
                axis: ScrollAxis::Vertical,
                line_height: 48.0,
            },
        )
        .expect("scrollable");
    app.set_scroll_content(&handle, Size::new(200.0, 400.0))
        .expect("content");
    settle(&mut app, window);
    set_focus(&mut app, window, inside);

    let action = Action::custom("unhandled");
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, action.clone()))
        .expect("bind");
    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty()),
    );
    assert_eq!(
        outcome.action, None,
        "没有 handler 的 scoped 命中不消费按键"
    );
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        48.0,
        "必须回落默认方向键滚动（一行 = line_height 48）"
    );

    let hits = hit_counter(&mut app, &action);
    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty()),
    );
    assert_eq!(hits.get(), 1);
    assert_eq!(outcome.action, Some(action));
    assert_eq!(
        app.scroll_state(&handle).expect("state").offset.y,
        48.0,
        "scoped 命中并消费后不得再走默认滚动"
    );
}

#[test]
fn scoped_key_registration_resolution_and_unbinding_do_not_request_frames() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = mount_scope(&mut app, window, root);
    let inside = mount_box(&mut app, window, scope);
    set_focus(&mut app, window, inside);
    settle(&mut app, window);

    let action = Action::custom("counted");
    let hits = hit_counter(&mut app, &action);
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, action.clone()))
        .expect("bind");
    assert!(!app.take_wake_request(), "注册 scoped 绑定不得请求帧");

    let outcome = app.dispatch_input(
        window,
        InputEvent::key_down(KeyCode::ArrowDown, Modifiers::empty()),
    );
    assert_eq!(
        outcome.action,
        Some(action),
        "命中必须真的分派到 handler（否则本用例没有走命中路径）"
    );
    assert_eq!(hits.get(), 1);
    assert!(
        !app.take_wake_request(),
        "命中（handler 无状态写入）不得请求帧"
    );
    assert!(
        matches!(
            app.frame(window).expect("window").expect("frame"),
            FrameOutcome::Idle
        ),
        "命中之后下一帧必须 Idle（没有脏位 / 没有帧请求）"
    );

    assert!(
        app.unbind_key_scoped(window, scope, KeyCode::ArrowDown, Modifiers::empty()),
        "绑定确实存在"
    );
    assert!(!app.take_wake_request(), "解绑不得请求帧");
    assert!(matches!(
        app.frame(window).expect("window").expect("frame"),
        FrameOutcome::Idle
    ));
}

#[test]
fn nested_scopes_resolve_from_the_inside_out() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let outer = mount_scope(&mut app, window, root);
    let inner = mount_scope(&mut app, window, outer);
    let deepest = mount_box(&mut app, window, inner);
    let outer_other = mount_box(&mut app, window, outer);

    let inner_action = Action::custom("inner");
    let outer_action = Action::custom("outer");
    let inner_hits = hit_counter(&mut app, &inner_action);
    let outer_hits = hit_counter(&mut app, &outer_action);
    app.bind_key_scoped(window, inner, binding(KeyCode::ArrowDown, inner_action))
        .expect("inner bind");
    app.bind_key_scoped(window, outer, binding(KeyCode::ArrowDown, outer_action))
        .expect("outer bind");

    set_focus(&mut app, window, deepest);
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(inner_hits.get(), 1);
    assert_eq!(outer_hits.get(), 0, "内层命中后不得再试外层");

    set_focus(&mut app, window, outer_other);
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(inner_hits.get(), 1);
    assert_eq!(outer_hits.get(), 1);

    app.off_action(&Action::custom("inner"));
    set_focus(&mut app, window, deepest);
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(inner_hits.get(), 1, "handler 已摘除");
    assert_eq!(outer_hits.get(), 2, "内层不处理时由外层接管");
}

#[test]
fn rebinding_the_same_scope_key_replaces_instead_of_growing() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = mount_scope(&mut app, window, root);
    let inside = mount_box(&mut app, window, scope);
    set_focus(&mut app, window, inside);

    let first = Action::custom("first");
    let second = Action::custom("second");
    let first_hits = hit_counter(&mut app, &first);
    let second_hits = hit_counter(&mut app, &second);
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, first.clone()))
        .expect("bind 1");
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, second.clone()))
        .expect("bind 2");
    assert_eq!(scoped_bindings(&app), 1, "重复绑定不得增长表项");

    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(first_hits.get(), 0);
    assert_eq!(second_hits.get(), 1, "后写覆盖");
}

#[test]
fn unbind_key_scoped_removes_exactly_one_binding() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = mount_scope(&mut app, window, root);
    let other_scope = mount_scope(&mut app, window, root);
    let inside = mount_box(&mut app, window, scope);
    let other_inside = mount_box(&mut app, window, other_scope);
    set_focus(&mut app, window, inside);

    let down = Action::custom("down");
    let up = Action::custom("up");
    let down_hits = hit_counter(&mut app, &down);
    let up_hits = hit_counter(&mut app, &up);
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, down.clone()))
        .expect("bind down");
    app.bind_key_scoped(window, scope, binding(KeyCode::ArrowUp, up.clone()))
        .expect("bind up");
    app.bind_key_scoped(
        window,
        other_scope,
        binding(KeyCode::ArrowDown, down.clone()),
    )
    .expect("bind other");

    assert!(app.unbind_key_scoped(window, scope, KeyCode::ArrowDown, Modifiers::empty()));
    assert!(
        !app.unbind_key_scoped(window, scope, KeyCode::ArrowDown, Modifiers::empty()),
        "重复解绑幂等"
    );
    assert_eq!(scoped_bindings(&app), 2, "只删一条（另一条 + 另一 scope）");

    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(down_hits.get(), 0, "解绑后不再触发");
    press(&mut app, window, KeyCode::ArrowUp);
    assert_eq!(up_hits.get(), 1, "同 scope 其它键不受影响");

    set_focus(&mut app, window, other_inside);
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(down_hits.get(), 1, "其它 scope 的同名绑定不受影响");
}

#[test]
fn unbind_scope_keys_removes_all_bindings_of_one_scope_only() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = mount_scope(&mut app, window, root);
    let other = mount_scope(&mut app, window, root);
    for (name, key) in [
        ("a.down", KeyCode::ArrowDown),
        ("a.up", KeyCode::ArrowUp),
        ("a.left", KeyCode::ArrowLeft),
    ] {
        app.bind_key_scoped(window, scope, binding(key, Action::custom(name)))
            .expect("bind");
    }
    app.bind_key_scoped(
        window,
        other,
        binding(KeyCode::ArrowDown, Action::custom("other")),
    )
    .expect("bind other");

    assert_eq!(app.unbind_scope_keys(window, scope), 3);
    assert_eq!(scoped_bindings(&app), 1, "其它 scope 保留");
    assert_eq!(app.unbind_scope_keys(window, scope), 0, "幂等");
}

#[test]
fn removing_a_scope_subtree_clears_its_bindings_and_keeps_the_ancestors() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let outer = mount_scope(&mut app, window, root);
    let inner = mount_scope(&mut app, window, outer);
    app.bind_key_scoped(
        window,
        outer,
        binding(KeyCode::ArrowDown, Action::custom("outer")),
    )
    .expect("outer bind");
    app.bind_key_scoped(
        window,
        inner,
        binding(KeyCode::ArrowDown, Action::custom("inner")),
    )
    .expect("inner bind");
    assert_eq!(scoped_bindings(&app), 2);

    let leaf = mount_box(&mut app, window, inner);
    assert!(app.remove_node(window, leaf).is_ok());
    assert_eq!(scoped_bindings(&app), 2, "卸载后代不影响 scope 绑定");

    assert!(app.remove_node(window, inner).is_ok());
    assert_eq!(scoped_bindings(&app), 1, "被卸载 scope 的绑定必须清除");

    let other = mount_box(&mut app, window, outer);
    set_focus(&mut app, window, other);
    let hits = {
        let hits = Rc::new(Cell::new(0));
        let counter = hits.clone();
        app.on_action(Action::custom("outer"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
        hits
    };
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(hits.get(), 1);
}

#[test]
fn closing_a_window_drops_only_its_scoped_bindings() {
    let (_rt, mut app, window) = setup();
    let second_window = app.create_window(WindowOptions::default());
    let first_root = root(&app, window);
    let second_root = root(&app, second_window);
    let first_scope = mount_scope(&mut app, window, first_root);
    let second_scope = mount_scope(&mut app, second_window, second_root);
    app.bind_key_scoped(
        window,
        first_scope,
        binding(KeyCode::ArrowDown, Action::custom("first")),
    )
    .expect("first bind");
    app.bind_key_scoped(
        second_window,
        second_scope,
        binding(KeyCode::ArrowDown, Action::custom("second")),
    )
    .expect("second bind");
    assert_eq!(scoped_bindings(&app), 2);

    assert!(app.close_window(window));
    assert_eq!(scoped_bindings(&app), 1, "关窗必须清空该窗口的 scoped 绑定");
}

#[test]
fn registration_rejects_unknown_windows_and_stale_scopes() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = mount_scope(&mut app, window, root);
    let action = Action::custom("act");

    let unknown = WindowId::for_test(9999);
    assert!(matches!(
        app.bind_key_scoped(unknown, scope, binding(KeyCode::ArrowDown, action.clone())),
        Err(EngineError::UnknownWindow { window: reported }) if reported == unknown
    ));
    assert_eq!(scoped_bindings(&app), 0);

    let second = app.create_window(WindowOptions::default());
    assert!(matches!(
        app.bind_key_scoped(second, scope, binding(KeyCode::ArrowDown, action.clone())),
        Err(EngineError::StaleNode { node }) if node == scope
    ));
    assert_eq!(scoped_bindings(&app), 0);

    assert!(app.remove_node(window, scope).is_ok());
    assert!(matches!(
        app.bind_key_scoped(window, scope, binding(KeyCode::ArrowDown, action)),
        Err(EngineError::StaleNode { .. })
    ));
    assert_eq!(scoped_bindings(&app), 0);
}

fn animation(duration: Duration) -> AnimationSpec {
    AnimationSpec {
        duration,
        easing: Easing::Linear,
    }
}

#[test]
fn releasing_a_scope_reclaims_scoped_transients_and_cancels_their_animations() {
    let (_rt, mut app, window) = setup();
    let scope = app.signal_scope();
    let fade: Transient<f32> = app.transient_scoped(scope, 0.0);
    let tint: Transient<Color> = app.transient_scoped(scope, Color::TRANSPARENT);
    let root_transient: Transient<f32> = app.transient(1.0);
    let _signal: Signal<u32> = app.signal_scoped(scope, 7);

    app.write_transient(&fade, 0.25);
    assert!((*app.read_transient(&fade) - 0.25).abs() < f32::EPSILON);

    let _root_animation = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: root_transient,
                to: 5.0,
            },
            &[],
            animation(Duration::from_millis(100)),
        )
        .expect("root animation");
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: fade,
            to: 1.0,
        },
        &[],
        animation(Duration::from_millis(100)),
    )
    .expect("fade animation");
    app.animate(
        window,
        AnimatedValue::Color {
            target: tint,
            to: Color::WHITE,
        },
        &[],
        animation(Duration::from_millis(100)),
    )
    .expect("color animation");
    assert_eq!(app.animations.len(), 3);

    assert_eq!(app.release_scope(scope), 3, "信号 1 + 瞬态值 2");
    assert_eq!(scoped_bindings(&app), 0, "无关的表不受影响");
    assert_eq!(
        app.animations.len(),
        1,
        "引用已释放瞬态值的动画必须同批取消"
    );
    assert!(app.animations_running(window), "根瞬态的动画仍然在飞");
    assert!(app.has_animations(), "只保留根作用域的动画");

    app.advance_clock(Duration::from_millis(200));
    app.frame(window).expect("window").expect("frame");
    assert!(!app.animations_running(window), "动画结束后清槽");
    assert!((*app.read_transient(&root_transient) - 5.0).abs() < 1e-4);

    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = *app.read_transient(&fade);
    }))
    .expect_err("已释放瞬态值的读必须 panic");
    assert!(panic_message(payload).contains("stale"));

    assert_eq!(app.release_scope(scope), 0);
}

#[test]
fn writing_a_released_scoped_transient_panics_like_reading_it() {
    let (_rt, mut app, _window) = setup();
    let scope = app.signal_scope();
    let fade: Transient<f32> = app.transient_scoped(scope, 0.0);
    let plain: Signal<u32> = app.signal_scoped(scope, 1);
    assert_eq!(app.release_scope(scope), 2, "瞬态值 + 普通信号");

    let write = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.write_transient(&fade, 1.0);
    }))
    .expect_err("已释放瞬态值的写必须 panic（不是 no-op）");
    let message = panic_message(write);
    assert!(message.contains("stale"), "panic 必须点名 stale：{message}");
    assert!(
        message.contains("write_transient"),
        "panic 必须区分瞬态写与普通信号写：{message}"
    );

    assert!(!app.write(&plain, 2), "普通信号的失效写是 no-op");

    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = *app.read_transient(&fade);
    }))
    .expect_err("已释放瞬态值的读必须 panic");
    assert!(panic_message(read).contains("stale"));
}

#[test]
#[should_panic(expected = "released scopes cannot mint")]
fn transient_scoped_rejects_a_released_scope() {
    let (_rt, mut app, _window) = setup();
    let scope = app.signal_scope();
    assert_eq!(app.release_scope(scope), 0);
    let _: Transient<f32> = app.transient_scoped(scope, 0.0);
}

#[test]
fn scoped_transient_churn_keeps_the_slot_peak_flat() {
    const CYCLES: usize = 1000;
    let (_rt, mut app, _window) = setup();
    let mut peak: Option<usize> = None;
    for cycle in 0..CYCLES {
        let scope = app.signal_scope();
        let fade: Transient<f32> = app.transient_scoped(scope, 0.0);
        let signal: Signal<u32> = app.signal_scoped(scope, 1);
        app.write_transient(&fade, cycle as f32);
        assert!(app.write(&signal, cycle as u32 + 2));
        assert_eq!(app.release_scope(scope), 2);
        assert_eq!(app.dependency_edge_count_for_test(), 0);
        let slots = app.store.slot_count_for_test();
        match peak {
            Some(expected) => assert_eq!(slots, expected, "第 {cycle} 轮槽位峰值不得增长"),
            None => peak = Some(slots),
        }
    }
    assert_eq!(app.store.live_slot_count_for_test(), 0, "没有存活槽位");
    assert_eq!(
        app.store.free_slot_count_for_test(),
        2,
        "两个槽位回到空闲栈"
    );
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        String::from("<non-string panic payload>")
    }
}

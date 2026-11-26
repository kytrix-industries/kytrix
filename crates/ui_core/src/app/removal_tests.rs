use std::{
    cell::{Cell, RefCell},
    panic::AssertUnwindSafe,
    rc::Rc,
    time::Duration,
};

use super::{AnimationEndKind, App};
use crate::{
    error::EngineError,
    event::{Action, Event, InputEvent, KeyBinding, KeyCode, Modifiers, PointerButton},
    interaction::{
        AnimatedValue, AnimationSpec, Easing, ScrollConfig, ScrollHandle, VirtualListConfig,
    },
    math::{DeviceRect, LogicalPoint, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        dirty::DirtyFlags,
        id::NodeId,
        signal::{Signal, Transient},
    },
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Quad, QuadPaint},
        style::{Clip, PaintSpec, ShadowSpec, Shadows, Style, StylePatch},
        style_builder::StyleBuilder,
    },
    window::{FrameOutcome, WindowId, WindowOptions},
};

fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    app.use_engine_clock();
    let window = app.create_window(WindowOptions {
        title: "e16".into(),
        size: LogicalSize::new(px(400.0), px(300.0)),
        ..WindowOptions::default()
    });
    (rt, app, window)
}

fn root(app: &App, window: WindowId) -> NodeId {
    app.window(window).expect("window").tree().root()
}

fn boxed_style(width: f32, height: f32) -> Style {
    StyleBuilder::new().size(px(width), px(height)).build()
}

fn mount_box(app: &mut App, window: WindowId, parent: NodeId) -> NodeId {
    NodeBuilder::boxed()
        .style(boxed_style(40.0, 40.0))
        .on_click(Action::custom("focusable"))
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("mount box")
}

fn mount_bound(app: &mut App, window: WindowId, parent: NodeId, signal: Signal<u32>) -> NodeId {
    NodeBuilder::boxed()
        .style(boxed_style(40.0, 40.0))
        .on_click(Action::custom("focusable"))
        .bind_style(move |cx| {
            let _ = *cx.read(&signal);
            StylePatch::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("mount bound")
}

fn mount_scroll(app: &mut App, window: WindowId, parent: NodeId) -> NodeId {
    let mut style = boxed_style(100.0, 100.0);
    style.clip = Clip::Bounds;
    NodeBuilder::boxed()
        .style(style)
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("mount scroll")
}

fn binding(key: KeyCode, action: Action) -> KeyBinding {
    KeyBinding {
        key,
        modifiers: Modifiers::empty(),
        action,
    }
}

fn animation(duration: Duration) -> AnimationSpec {
    AnimationSpec {
        duration,
        easing: Easing::Linear,
    }
}

fn scoped_count(app: &App, window: WindowId, scope: NodeId) -> usize {
    app.scoped_key_binding_count(window, scope)
}

fn timer_count(app: &App) -> usize {
    app.timer_count_for_test()
}

fn animation_count(app: &App) -> usize {
    app.animations.len()
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

fn press(app: &mut App, window: WindowId, code: KeyCode) {
    app.dispatch_input(window, InputEvent::key_down(code, Modifiers::empty()));
}

#[test]
fn removing_a_subtree_cleans_bindings_timers_and_animations_with_counts() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let outer = mount_box(&mut app, window, root);
    let inner = mount_box(&mut app, window, outer);
    let leaf_a = mount_box(&mut app, window, inner);
    let leaf_b = mount_box(&mut app, window, inner);
    let outside = mount_box(&mut app, window, root);
    for (scope, name) in [
        (outer, "outer"),
        (inner, "inner"),
        (leaf_a, "leaf"),
        (outside, "outside"),
    ] {
        app.bind_key_scoped(
            window,
            scope,
            binding(KeyCode::ArrowDown, Action::custom(name)),
        )
        .expect("bind");
    }
    assert_eq!(scoped_count(&app, window, inner), 1);

    let inside_hits = Rc::new(Cell::new(0u32));
    let inside_sink = Rc::clone(&inside_hits);
    app.after_scoped(window, inner, Duration::from_millis(500), move |_app| {
        inside_sink.set(inside_sink.get() + 1);
    })
    .expect("inside timer");
    let outside_hits = Rc::new(Cell::new(0u32));
    let outside_sink = Rc::clone(&outside_hits);
    app.after_scoped(window, outside, Duration::from_millis(500), move |_app| {
        outside_sink.set(outside_sink.get() + 1);
    })
    .expect("outside timer");
    assert_eq!(timer_count(&app), 2);

    let fade = app.transient(0.0_f32);
    let tint = app.transient(0.0_f32);
    let shared = app.transient(0.0_f32);
    let cancelled = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: fade,
                to: 1.0,
            },
            &[leaf_a, leaf_b],
            animation(Duration::from_millis(100)),
        )
        .expect("inside animation");
    let _kept = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: tint,
                to: 1.0,
            },
            &[outside],
            animation(Duration::from_millis(100)),
        )
        .expect("outside animation");
    let shared_id = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: shared,
                to: 1.0,
            },
            &[leaf_a, outside],
            animation(Duration::from_millis(100)),
        )
        .expect("shared animation");
    assert_eq!(animation_count(&app), 3);

    let completions: Rc<RefCell<Vec<(crate::interaction::AnimationId, AnimationEndKind)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&completions);
    app.on_animation_complete(move |_app, event| sink.borrow_mut().push((event.id, event.kind)));

    let outcome = app
        .remove_node(window, inner)
        .expect("remove inner subtree");
    assert_eq!(outcome.removed_nodes, 3, "inner + leaf_a + leaf_b");
    assert_eq!(
        outcome.scoped_key_bindings, 2,
        "inner 与后代 leaf_a 的 scope 绑定"
    );
    assert_eq!(outcome.timers, 1);
    assert_eq!(outcome.animations, 1, "只有重绘节点全落在子树内的槽被取消");
    assert_eq!(outcome.released_values, 0, "无作用域参数");

    assert_eq!(scoped_count(&app, window, inner), 0);
    assert_eq!(
        scoped_count(&app, window, leaf_a),
        0,
        "后代 scope 的绑定同批清除"
    );
    assert_eq!(
        scoped_count(&app, window, outer),
        1,
        "祖先 scope 的绑定保留"
    );
    assert_eq!(
        scoped_count(&app, window, outside),
        1,
        "兄弟 scope 的绑定保留"
    );
    assert_eq!(timer_count(&app), 1, "只取消子树 scope 的定时器");
    assert_eq!(animation_count(&app), 2);
    let shared_slot = app
        .animations
        .iter()
        .find(|slot| slot.id == shared_id.generation())
        .expect("共享动画必须保留（部分重绘节点存活）");
    assert_eq!(
        shared_slot.nodes,
        vec![outside],
        "死节点必须从共享动画的重绘列表里剔除"
    );

    app.run_frame_hook();
    assert_eq!(
        completions.borrow().as_slice(),
        &[(cancelled, AnimationEndKind::Removed)],
        "只有被卸载路径取消的动画报告 Removed"
    );

    app.advance_clock(Duration::from_millis(600));
    app.run_frame_hook();
    assert_eq!(inside_hits.get(), 0, "被卸载 scope 的定时器不得再回调");
    assert_eq!(outside_hits.get(), 1, "无关定时器不得被误伤");
}

#[test]
fn animations_that_only_drive_surviving_nodes_are_untouched() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let removed = mount_box(&mut app, window, root);
    let kept = mount_box(&mut app, window, root);
    let value = app.transient(0.0_f32);
    let id = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: value,
                to: 1.0,
            },
            &[kept],
            animation(Duration::from_millis(100)),
        )
        .expect("animate");

    let outcome = app.remove_node(window, removed).expect("remove unrelated");
    assert_eq!(outcome.animations, 0);
    assert_eq!(animation_count(&app), 1);
    assert!(
        app.animations
            .iter()
            .any(|slot| slot.id == id.generation() && slot.nodes == vec![kept]),
        "无关动画保持原样"
    );
}

#[test]
fn scroll_animations_are_cancelled_with_their_container() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = mount_scroll(&mut app, window, root);
    let handle = app
        .make_scroll(window, container, ScrollConfig::default())
        .expect("scroll container");
    app.set_scroll_content(&handle, LogicalSize::new(px(100.0), px(400.0)))
        .expect("content");
    app.scroll_to_animated(&handle, LogicalPoint::new(px(0.0), px(120.0)))
        .expect("animated scroll");
    assert_eq!(animation_count(&app), 1);
    assert!(app.needs_continuous_frames(), "滚动动画需要帧");

    let outcome = app
        .remove_node(window, container)
        .expect("remove container");
    assert_eq!(outcome.animations, 1);
    assert_eq!(animation_count(&app), 0, "滚动动画槽随容器取消");
    assert!(
        !app.needs_continuous_frames(),
        "取消后不得再因这条动画进入连续帧（I3）"
    );
    assert_eq!(
        app.pending_animation_completions_for_test(),
        0,
        "滚动槽是引擎内部驱动，不产生公开完成事件"
    );
}

#[test]
fn virtual_list_row_recycling_cleans_scoped_state_of_recycled_rows() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let mut style = boxed_style(200.0, 100.0);
    style.clip = Clip::Bounds;
    style.position(taffy::style::Position::Relative);
    let container = NodeBuilder::boxed()
        .style(style)
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("virtual container");
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |_index| {
        NodeBuilder::boxed().layout_override(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(200.0),
            px(25.0),
        ))
    })
    .expect("items");
    settle(&mut app, window);

    let row = app
        .window(window)
        .expect("window")
        .tree()
        .virtual_slot(container)
        .expect("virtual slot")
        .mounted
        .iter()
        .find(|(index, _)| *index == 0)
        .map(|(_, row)| *row)
        .expect("第 0 行必须已挂载");
    app.bind_key_scoped(
        window,
        row,
        binding(KeyCode::ArrowDown, Action::custom("row")),
    )
    .expect("bind row");
    app.after_scoped(window, row, Duration::from_secs(60), |_app| {})
        .expect("row timer");
    let value = app.transient(0.0_f32);
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[row],
        animation(Duration::from_millis(100)),
    )
    .expect("row animation");
    assert_eq!(scoped_count(&app, window, row), 1);
    assert_eq!(timer_count(&app), 1);
    assert_eq!(animation_count(&app), 1);

    app.scroll_to(
        &ScrollHandle::new(window, container),
        LogicalPoint::new(px(0.0), px(2000.0)),
    )
    .expect("scroll far");
    assert_eq!(
        scoped_count(&app, window, row),
        0,
        "回收行的 scoped 绑定必须清除"
    );
    assert_eq!(timer_count(&app), 0, "回收行的定时器必须取消");
    assert_eq!(animation_count(&app), 0, "回收行的动画槽必须取消");
}

#[test]
fn removing_an_ancestor_cleans_descendants_and_repeat_removal_is_stale() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let outer = mount_box(&mut app, window, root);
    let inner = mount_box(&mut app, window, outer);
    let leaf = mount_box(&mut app, window, inner);
    app.bind_key_scoped(
        window,
        inner,
        binding(KeyCode::ArrowDown, Action::custom("inner")),
    )
    .expect("bind");
    app.after_scoped(window, inner, Duration::from_secs(60), |_app| {})
        .expect("timer");
    let value = app.transient(0.0_f32);
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[leaf],
        animation(Duration::from_millis(100)),
    )
    .expect("animate");

    let outcome = app.remove_node(window, leaf).expect("remove leaf");
    assert_eq!(outcome.removed_nodes, 1);
    assert_eq!(outcome.scoped_key_bindings, 0);
    assert_eq!(outcome.timers, 0);
    assert_eq!(outcome.animations, 1, "动画重绘节点被卸载 ⇒ 取消");
    assert_eq!(scoped_count(&app, window, inner), 1);
    assert_eq!(timer_count(&app), 1);
    assert_eq!(animation_count(&app), 0);

    assert!(matches!(
        app.remove_node(window, leaf),
        Err(EngineError::StaleNode { node }) if node == leaf
    ));
    assert_eq!(scoped_count(&app, window, inner), 1);
    assert_eq!(timer_count(&app), 1);
    assert_eq!(animation_count(&app), 0);

    let value = app.transient(0.0_f32);
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[inner],
        animation(Duration::from_millis(100)),
    )
    .expect("animate inner");
    let outcome = app.remove_node(window, inner).expect("remove inner");
    assert_eq!(outcome.removed_nodes, 1);
    assert_eq!(outcome.scoped_key_bindings, 1);
    assert_eq!(outcome.timers, 1);
    assert_eq!(outcome.animations, 1);
    assert_eq!(scoped_count(&app, window, inner), 0);
    assert_eq!(timer_count(&app), 0);
    assert_eq!(animation_count(&app), 0);
    assert!(
        app.window(window)
            .expect("window")
            .tree()
            .get(outer)
            .is_ok(),
        "祖先节点不得被误删"
    );
}

#[test]
fn removal_errors_are_strongly_typed_and_side_effect_free() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let node = mount_box(&mut app, window, root);
    app.bind_key_scoped(
        window,
        node,
        binding(KeyCode::ArrowDown, Action::custom("x")),
    )
    .expect("bind");
    app.after_scoped(window, node, Duration::from_secs(60), |_app| {})
        .expect("timer");
    let value = app.transient(0.0_f32);
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[node],
        animation(Duration::from_millis(100)),
    )
    .expect("animate");
    let before = (
        app.scoped_bindings.len(),
        timer_count(&app),
        animation_count(&app),
        app.window(window).expect("window").tree().len(),
    );

    let unknown = WindowId::for_test(9999);
    assert!(matches!(
        app.remove_node(unknown, node),
        Err(EngineError::UnknownWindow { window: reported }) if reported == unknown
    ));
    assert!(
        matches!(app.remove_node(window, root), Err(EngineError::StaleNode { node: reported }) if reported == root),
        "root 不是可移除的子节点"
    );
    let second = app.create_window(WindowOptions::default());
    assert!(
        matches!(app.remove_node(second, node), Err(EngineError::StaleNode { node: reported }) if reported == node),
        "跨窗口句柄必须在注册期失败（StaleNode）"
    );

    let after = (
        app.scoped_bindings.len(),
        timer_count(&app),
        animation_count(&app),
        app.window(window).expect("window").tree().len(),
    );
    assert_eq!(before, after, "错误路径不得产生任何清理 / 结构副作用");
    assert!(
        app.window(window).expect("window").tree().get(node).is_ok(),
        "误用不得影响真正宿主窗口的节点"
    );
}

#[test]
fn remove_node_with_scope_releases_values_after_unloading() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope = app.signal_scope();
    let signal: Signal<u32> = app.signal_scoped(scope, 7);
    let fade: Transient<f32> = app.transient_scoped(scope, 0.0);
    let node = mount_bound(&mut app, window, root, signal);
    app.refresh_styles(window).expect("window");
    assert_eq!(app.dependency_edge_count_for_test(), 1);

    let id = app
        .animate(
            window,
            AnimatedValue::Scalar {
                target: fade,
                to: 1.0,
            },
            &[],
            animation(Duration::from_millis(100)),
        )
        .expect("animate");
    let completions: Rc<RefCell<Vec<(crate::interaction::AnimationId, AnimationEndKind)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&completions);
    app.on_animation_complete(move |_app, event| sink.borrow_mut().push((event.id, event.kind)));

    let outcome = app
        .remove_node_with_scope(window, node, scope)
        .expect("remove + release");
    assert_eq!(outcome.removed_nodes, 1);
    assert_eq!(outcome.released_values, 2, "作用域内 1 信号 + 1 瞬态值");
    assert_eq!(outcome.animations, 1, "引用被回收瞬态值的动画同批取消");
    assert_eq!(
        app.dependency_edge_count_for_test(),
        0,
        "依赖边随节点卸载解除"
    );
    assert!(!app.animations_running(window));
    app.run_frame_hook();
    assert_eq!(
        completions.borrow().as_slice(),
        &[(id, AnimationEndKind::Released)]
    );

    let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = *app.read(&signal);
    }))
    .expect_err("已释放作用域的读必须 panic");
    assert!(
        payload
            .downcast_ref::<String>()
            .is_some_and(|text| text.contains("stale")),
        "panic 必须点名陈旧句柄"
    );

    assert!(matches!(
        app.remove_node_with_scope(window, node, scope),
        Err(EngineError::StaleNode { .. })
    ));
    let survivor = mount_box(&mut app, window, root);
    let outcome = app
        .remove_node_with_scope(window, survivor, scope)
        .expect("idempotent scope release");
    assert_eq!(outcome.released_values, 0, "重复释放幂等");
}

#[test]
fn action_handlers_and_global_bindings_are_application_owned() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let node = mount_box(&mut app, window, root);
    let scoped_action = Action::custom("scoped");
    let scoped_hits = Rc::new(Cell::new(0u32));
    let sink = Rc::clone(&scoped_hits);
    app.on_action(scoped_action.clone(), move |_ctx| {
        sink.set(sink.get() + 1);
    });
    app.bind_key_scoped(window, node, binding(KeyCode::ArrowDown, scoped_action))
        .expect("bind scoped");
    assert!(app.set_focus(window, Some(node)));
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(scoped_hits.get(), 1, "前置条件：scoped 绑定命中");

    let action = Action::custom("activate");
    app.on_action(action.clone(), |_ctx| {});
    app.bind_key(KeyBinding {
        key: KeyCode::KeyS,
        modifiers: Modifiers::CONTROL,
        action: action.clone(),
    });
    let global_bindings = app.bindings.len();
    assert!(app.has_action_handler(&action));

    let outcome = app.remove_node(window, node).expect("remove node");
    assert_eq!(outcome.scoped_key_bindings, 1);
    assert!(
        app.has_action_handler(&action),
        "全局 Action handler 必须保留（引擎无法归属到子树）"
    );
    assert_eq!(app.bindings.len(), global_bindings, "全局键绑定保留");
    assert_eq!(app.scoped_bindings.len(), 0, "scoped 绑定清空");
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(scoped_hits.get(), 1, "被卸载 scope 的绑定不得再命中");
    assert!(app.off_action(&action), "调用方仍可显式摘除 handler");
}

#[test]
fn thousand_node_lifecycles_keep_every_table_flat() {
    const CYCLES: usize = 1000;
    const VALUES_PER_SCOPE: usize = 2;

    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let mut peak: Option<usize> = None;
    for cycle in 0..CYCLES {
        let node = mount_box(&mut app, window, root);
        let child = mount_box(&mut app, window, node);
        let scope = app.signal_scope();
        let signal: Signal<u32> = app.signal_scoped(scope, 1);
        let fade: Transient<f32> = app.transient_scoped(scope, 0.0);
        let bound = mount_bound(&mut app, window, child, signal);
        app.refresh_styles(window).expect("window");
        assert_eq!(
            app.dependency_edge_count_for_test(),
            1,
            "第 {cycle} 轮：绑定节点对作用域信号有一条依赖边"
        );
        app.bind_key_scoped(
            window,
            node,
            binding(KeyCode::ArrowDown, Action::custom("cycle")),
        )
        .expect("bind");
        app.after_scoped(window, node, Duration::from_secs(60), |_app| {})
            .expect("timer");
        app.animate(
            window,
            AnimatedValue::Scalar {
                target: fade,
                to: 1.0,
            },
            &[bound],
            animation(Duration::from_millis(100)),
        )
        .expect("animate");

        let outcome = app
            .remove_node_with_scope(window, node, scope)
            .expect("remove + release");
        assert_eq!(
            outcome.removed_nodes, 3,
            "第 {cycle} 轮：node + child + bound"
        );
        assert_eq!(outcome.scoped_key_bindings, 1, "第 {cycle} 轮");
        assert_eq!(outcome.timers, 1, "第 {cycle} 轮");
        assert_eq!(outcome.animations, 1, "第 {cycle} 轮");
        assert_eq!(outcome.released_values, VALUES_PER_SCOPE, "第 {cycle} 轮");

        app.run_frame_hook();
        assert_eq!(app.scoped_bindings.len(), 0, "第 {cycle} 轮");
        assert_eq!(timer_count(&app), 0, "第 {cycle} 轮");
        assert_eq!(animation_count(&app), 0, "第 {cycle} 轮");
        assert_eq!(
            app.pending_animation_completions_for_test(),
            0,
            "第 {cycle} 轮"
        );
        assert_eq!(app.dependency_edge_count_for_test(), 0, "第 {cycle} 轮");
        assert_eq!(app.store.live_slot_count_for_test(), 0, "第 {cycle} 轮");
        assert_eq!(
            app.store.free_slot_count_for_test(),
            VALUES_PER_SCOPE,
            "第 {cycle} 轮：两个值槽位必须回到空闲栈"
        );
        let slots = app.store.slot_count_for_test();
        match peak {
            Some(expected) => assert_eq!(slots, expected, "第 {cycle} 轮：槽位峰值不得增长"),
            None => peak = Some(slots),
        }
    }
    assert_eq!(peak, Some(VALUES_PER_SCOPE));
}

#[test]
fn close_window_path_is_unchanged() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let node = mount_box(&mut app, window, root);
    let window_signal: Signal<u32> = app.window_signal(window, 5);
    app.bind_key_scoped(
        window,
        node,
        binding(KeyCode::ArrowDown, Action::custom("x")),
    )
    .expect("bind");
    app.after_scoped(window, node, Duration::from_secs(60), |_app| {})
        .expect("timer");
    let value = app.transient(0.0_f32);
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[node],
        animation(Duration::from_millis(100)),
    )
    .expect("animate");
    assert_eq!(app.scoped_bindings.len(), 1);
    assert_eq!(timer_count(&app), 1);
    assert_eq!(animation_count(&app), 1);

    assert!(app.close_window(window), "关窗成功");
    assert_eq!(app.scoped_bindings.len(), 0, "关窗清空该窗口的 scoped 绑定");
    assert_eq!(timer_count(&app), 0, "关窗取消该窗口的定时器");
    assert_eq!(animation_count(&app), 0, "关窗丢弃该窗口的动画槽");
    assert_eq!(app.next_wake_deadline(), None);
    assert!(
        !app.write(&window_signal, 6),
        "窗口作用域已释放：迟到写必须是 no-op"
    );
    assert!(matches!(
        app.remove_node(window, node),
        Err(EngineError::UnknownWindow { .. })
    ));
    assert!(!app.close_window(window), "重复关窗仍是 false");
}

#[test]
fn cleanup_leaves_no_frame_sources_behind() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = mount_box(&mut app, window, root);
    let leaf = mount_box(&mut app, window, container);
    app.bind_key_scoped(
        window,
        leaf,
        binding(KeyCode::ArrowDown, Action::custom("x")),
    )
    .expect("bind");
    app.after_scoped(window, leaf, Duration::from_secs(60), |_app| {})
        .expect("timer");
    let value = app.transient(0.0_f32);
    app.animate(
        window,
        AnimatedValue::Scalar {
            target: value,
            to: 1.0,
        },
        &[leaf],
        animation(Duration::from_millis(100)),
    )
    .expect("animate");

    let outcome = app.remove_node(window, container).expect("remove");
    assert_eq!(outcome.animations, 1);
    assert_eq!(outcome.timers, 1);

    settle(&mut app, window);
    assert!(!app.take_wake_request(), "清理之后不得残留帧请求");
    assert!(!app.needs_continuous_frames(), "动画已取消 ⇒ 不得连续出帧");
    assert_eq!(
        app.next_wake_deadline(),
        None,
        "定时器已取消 ⇒ 无唤醒截止时刻"
    );
    assert_eq!(timer_count(&app), 0);
    assert_eq!(animation_count(&app), 0);
}

#[test]
fn removing_a_subtree_requests_the_next_frame() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = mount_box(&mut app, window, root);
    let _leaf = mount_box(&mut app, window, container);

    let mut last_committed = None;
    for _ in 0..8 {
        match app.frame(window).expect("window").expect("frame") {
            FrameOutcome::Committed(index) => last_committed = Some(index),
            FrameOutcome::Idle => break,
        }
    }
    let last_committed = last_committed.expect("挂载必须至少提交一帧");
    let _ = app.drain_events();
    assert!(!app.take_wake_request(), "静止窗口没有待办帧");
    assert!(
        matches!(
            app.frame(window).expect("window").expect("frame"),
            FrameOutcome::Idle
        ),
        "前置条件：窗口已静止（Idle）"
    );

    app.remove_node(window, container).expect("remove subtree");
    assert!(
        app.take_wake_request(),
        "删子树必须请求一帧（F2：删掉「请求一帧」整步会让画面停在旧像素）"
    );
    let outcome = app.frame(window).expect("window").expect("frame");
    assert!(
        matches!(outcome, FrameOutcome::Committed(index) if index.get() == last_committed.get() + 1),
        "删子树后的下一帧必须提交且 FrameIndex + 1，得到 {outcome:?}"
    );
}

#[test]
fn removing_a_hovered_subtree_derives_a_leave_event() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let keep = NodeBuilder::boxed()
        .style(boxed_style(120.0, 120.0))
        .bind_style(|_| StylePatch::default())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount keep");
    let outer = mount_box(&mut app, window, keep);
    let inner = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(10.0), px(20.0), px(20.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("mount inner");
    settle(&mut app, window);
    let _ = app.drain_events();

    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(20.0), px(20.0)), Modifiers::empty()),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ),
    );
    let _ = app.drain_events();
    assert_eq!(app.hovered(window), Some(inner), "前置条件：inner 被悬停");
    assert_eq!(
        app.windows
            .get(&window)
            .expect("window")
            .pointer()
            .press
            .and_then(|press| press.node),
        Some(inner),
        "前置条件：press 指向 inner"
    );
    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .clear_all_dirty_for_test();

    let outcome = app
        .remove_node(window, outer)
        .expect("remove hovered subtree");
    assert_eq!(outcome.removed_nodes, 2, "outer + inner");

    assert_eq!(app.hovered(window), None, "被卸载的悬停必须结束");
    let pointer = app.windows.get(&window).expect("window").pointer();
    assert!(pointer.hovered.is_none(), "死句柄不得留在悬停槽里");
    assert!(pointer.press.is_none(), "落在被删子树的按压记录必须清掉");
    assert!(pointer.last_hit.is_none(), "落在被删子树的命中记录必须清掉");
    let hover_events: Vec<(Option<NodeId>, Vec<NodeId>)> = app
        .drain_events()
        .into_iter()
        .filter_map(|(_, event)| match event {
            Event::HoverChanged { hovered, left, .. } => Some((hovered, left)),
            _ => None,
        })
        .collect();
    assert_eq!(
        hover_events.len(),
        1,
        "删除悬停子树必须恰好派生一条 HoverChanged：{hover_events:?}"
    );
    assert_eq!(hover_events[0].0, None, "删除后没有可判定的悬停目标");
    assert_eq!(
        hover_events[0].1,
        vec![inner, outer, keep, root],
        "leave 链由内到外且包含根（与 update_hover 的 leave 口径一致）"
    );
    assert!(
        app.window(window)
            .expect("window")
            .tree()
            .get(keep)
            .expect("keep")
            .dirty()
            .contains(DirtyFlags::BIND),
        "存活祖先失去 :hover ⇒ 必须标 BIND 重算 bind_style"
    );

    let other = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(250.0), px(10.0), px(40.0), px(40.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount other");
    settle(&mut app, window);
    let _ = app.drain_events();
    app.dispatch_input(
        window,
        InputEvent::pointer_move(LogicalPoint::new(px(260.0), px(20.0)), Modifiers::empty()),
    );
    let _ = app.drain_events();
    assert_eq!(app.hovered(window), Some(other), "前置条件：other 被悬停");
    app.remove_node(window, keep).expect("删除无关子树");
    assert_eq!(app.hovered(window), Some(other), "无关悬停不得被打扰");
    assert!(
        !app.drain_events()
            .into_iter()
            .any(|(_, event)| matches!(event, Event::HoverChanged { .. })),
        "悬停不在被删子树内时不得派生 HoverChanged"
    );
}

#[test]
fn removing_a_mounted_virtual_row_is_rejected_without_side_effects() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let mut style = boxed_style(200.0, 100.0);
    style.clip = Clip::Bounds;
    style.position(taffy::style::Position::Relative);
    let container = NodeBuilder::boxed()
        .style(style)
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("virtual container");
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |_index| {
        NodeBuilder::boxed().layout_override(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(200.0),
            px(25.0),
        ))
    })
    .expect("items");
    settle(&mut app, window);
    let mounted = app
        .window(window)
        .expect("window")
        .tree()
        .virtual_slot(container)
        .expect("virtual slot")
        .mounted
        .clone();
    let row = mounted
        .iter()
        .find(|(index, _)| *index == 0)
        .map(|(_, row)| *row)
        .expect("第 0 行必须已挂载");

    let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = app.remove_node(window, row);
    }))
    .expect_err("删除已挂载虚拟行必须在 debug 构建里硬失败");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
        })
        .unwrap_or_default();
    assert!(
        message.contains("已挂载行"),
        "panic 必须点名虚拟行契约：{message}"
    );

    assert!(
        app.window(window).expect("window").tree().get(row).is_ok(),
        "被拒绝的删除不得触碰节点树"
    );
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .virtual_slot(container)
            .expect("virtual slot")
            .mounted,
        mounted,
        "被拒绝的删除不得改写 mounted 索引"
    );

    let outcome = app
        .remove_node(window, container)
        .expect("删除虚拟列表容器本身是合法操作");
    assert_eq!(outcome.removed_nodes, 1 + mounted.len(), "容器 + 已挂载行");
    let tree = app.window(window).expect("window").tree();
    assert!(tree.get(container).is_err(), "容器已卸载");
    assert!(
        tree.virtual_slot(container).is_none(),
        "容器登记必须随卸载清理"
    );
}

#[test]
fn remove_node_with_scope_rejects_provable_scope_mismatch() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let scope_a = app.signal_scope();
    let signal_a: Signal<u32> = app.signal_scoped(scope_a, 7);
    let node_a = mount_bound(&mut app, window, root, signal_a);
    app.refresh_styles(window).expect("window");
    assert_eq!(app.dependency_edge_count_for_test(), 1);
    let node_b = mount_box(&mut app, window, root);
    let tree_len = app.window(window).expect("window").tree().len();
    let live_slots = app.store.live_slot_count_for_test();

    let mismatch = app
        .remove_node_with_scope(window, node_b, scope_a)
        .expect_err("子树外仍有存活读者 ⇒ 可证明的错配必须拒绝");
    assert!(
        matches!(
            mismatch,
            EngineError::ScopeOwnershipMismatch { node, scope, reader }
                if node == node_b && scope == scope_a && reader == node_a
        ),
        "错误必须点名子树根 / 作用域 / 子树外的存活读者，得到 {mismatch:?}"
    );
    assert_eq!(app.window(window).expect("window").tree().len(), tree_len);
    assert_eq!(*app.read(&signal_a), 7, "错误路径不得释放作用域");
    assert_eq!(app.dependency_edge_count_for_test(), 1);
    assert_eq!(app.store.live_slot_count_for_test(), live_slots);

    let scope_b = app.signal_scope();
    let bound_signal: Signal<u32> = app.signal_scoped(scope_b, 1);
    let node_c = mount_bound(&mut app, window, root, bound_signal);
    app.refresh_styles(window).expect("window");
    let outcome = app
        .remove_node_with_scope(window, node_c, scope_b)
        .expect("正确归属的作用域");
    assert_eq!(outcome.released_values, 1);

    let scope_live = app.signal_scope();
    let signal_live: Signal<u32> = app.signal_scoped(scope_live, 3);
    let stale = mount_box(&mut app, window, root);
    app.remove_node(window, stale).expect("先删一次");
    assert!(matches!(
        app.remove_node_with_scope(window, stale, scope_live),
        Err(EngineError::StaleNode { node }) if node == stale
    ));
    assert_eq!(*app.read(&signal_live), 3, "StaleNode 路径不得释放 scope");
}

fn frame_once(app: &mut App, window: WindowId) {
    let _ = app.frame(window).expect("window").expect("frame");
}

fn painted_extent(app: &App, window: WindowId, node: NodeId) -> LogicalRect {
    app.window(window)
        .expect("window")
        .tree()
        .painted_extent(node)
        .expect("已绘制节点必须记录 painted_extent")
        .layout_bounds()
}

fn latest_record(app: &App, window: WindowId) -> crate::test_support::record::FrameRecord {
    app.window(window)
        .expect("window")
        .record()
        .expect("提交帧必须留下 FrameRecord")
        .clone()
}

fn assert_damage_covers_extent(
    app: &App,
    window: WindowId,
    damage: &[DeviceRect],
    extent: LogicalRect,
    label: &str,
) {
    let dpr = app
        .window(window)
        .expect("window")
        .device_pixel_ratio()
        .factor();
    let max = extent.max();
    let center = LogicalPoint::new(
        extent.origin.x + extent.size.width / 2.0,
        extent.origin.y + extent.size.height / 2.0,
    );
    for point in [
        LogicalPoint::new(extent.origin.x + px(0.5), extent.origin.y + px(0.5)),
        LogicalPoint::new(max.x - px(0.5), extent.origin.y + px(0.5)),
        LogicalPoint::new(extent.origin.x + px(0.5), max.y - px(0.5)),
        LogicalPoint::new(max.x - px(0.5), max.y - px(0.5)),
        center,
    ] {
        let x = (point.x.value() * dpr).floor() as i32;
        let y = (point.y.value() * dpr).floor() as i32;
        assert!(
            damage.iter().any(|rect| {
                rect.x0.value() <= x
                    && x < rect.x1.value()
                    && rect.y0.value() <= y
                    && y < rect.y1.value()
            }),
            "{label} extent {extent:?} 的角点 {point:?} 必须被 damage 覆盖：{damage:?}"
        );
    }
}

fn shadowed_paint() -> PaintSpec {
    PaintSpec {
        background: Color::WHITE,
        shadows: Shadows::from(ShadowSpec {
            offset: LogicalPoint::new(px(0.0), px(6.0)),
            blur: px(12.0),
            spread: px(0.0),
            color: Color::BLACK,
        }),
        ..PaintSpec::default()
    }
}

fn positioned(rect: LogicalRect) -> Style {
    let mut style = StyleBuilder::new()
        .absolute()
        .size(rect.size.width, rect.size.height)
        .build();
    style.inset(
        taffy::style::LengthPercentageAuto::length(rect.origin.y.value()),
        taffy::style::LengthPercentageAuto::length(0.0),
        taffy::style::LengthPercentageAuto::length(0.0),
        taffy::style::LengthPercentageAuto::length(rect.origin.x.value()),
    );
    style
}

#[test]
fn removing_a_painted_subtree_damages_every_extent_without_a_full_frame() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(100.0),
            px(80.0),
        )))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("container");
    let panel = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(90.0),
            px(70.0),
            px(80.0),
            px(60.0),
        )))
        .paint(shadowed_paint())
        .build(
            app.window_mut(window).expect("window").tree_mut(),
            container,
        )
        .expect("panel");
    let mut canvas = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(30.0)));
    canvas.push_quad(Quad {
        bounds: canvas.bounds(),
        corner_radius: [px(0.0); 4],
        paint: QuadPaint::Solid {
            fill: Color::WHITE,
            border: None,
        },
    });
    let chart = NodeBuilder::canvas(canvas)
        .style(positioned(LogicalRect::new(
            px(10.0),
            px(10.0),
            px(20.0),
            px(10.0),
        )))
        .build(app.window_mut(window).expect("window").tree_mut(), panel)
        .expect("chart");
    frame_once(&mut app, window);

    let panel_extent = painted_extent(&app, window, panel);
    let chart_extent = painted_extent(&app, window, chart);
    assert!(
        panel_extent.max().y >= px(155.0),
        "面板 extent 必须含阴影 blur 外溢（盒底 130 + offset 6 + pad 19）：{panel_extent:?}"
    );
    assert!(
        chart_extent.size.width >= px(40.0) && chart_extent.size.height >= px(30.0),
        "Canvas extent 必须是画布内容外延：{chart_extent:?}"
    );
    assert!(
        panel_extent.max().x > px(100.0) && panel_extent.max().y > px(80.0),
        "前置条件：面板绘制外延必须溢出父容器盒，才能区分 extent 与 layout.bounds：{panel_extent:?}"
    );

    let outcome = app.remove_node(window, panel).expect("remove panel");
    assert_eq!(outcome.removed_nodes, 2, "面板 + 画布");
    frame_once(&mut app, window);

    let record = latest_record(&app, window);
    assert!(!record.damage_full, "删除子树不得退化为整帧");
    assert_damage_covers_extent(&app, window, &record.damage, panel_extent, "面板旧");
    assert_damage_covers_extent(&app, window, &record.damage, chart_extent, "画布旧");
}

#[test]
fn moving_a_shadowed_panel_damages_the_old_and_new_extents_without_a_full_frame() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let panel = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(20.0),
            px(20.0),
            px(120.0),
            px(80.0),
        )))
        .paint(shadowed_paint())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("panel");
    frame_once(&mut app, window);
    let old_extent = painted_extent(&app, window, panel);
    assert!(
        old_extent.max().y >= px(125.0),
        "旧 extent 必须含阴影外溢：{old_extent:?}"
    );

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_absolute_inset(panel, LogicalPoint::new(px(60.0), px(50.0)));
    frame_once(&mut app, window);

    let record = latest_record(&app, window);
    assert!(!record.damage_full, "移动面板不得退化为整帧");
    let new_extent = painted_extent(&app, window, panel);
    assert!(
        new_extent.origin.x >= px(40.0) && new_extent.origin.y >= px(35.0),
        "新 extent 必须跟随新位置（含阴影外溢）：{new_extent:?}"
    );
    assert_damage_covers_extent(&app, window, &record.damage, old_extent, "旧面板");
    assert_damage_covers_extent(&app, window, &record.damage, new_extent, "新面板");
}

#[test]
fn inserting_a_shadowed_subtree_paints_incrementally() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    frame_once(&mut app, window);
    let panel = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(120.0),
            px(90.0),
            px(160.0),
            px(100.0),
        )))
        .paint(shadowed_paint())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("panel");
    frame_once(&mut app, window);

    let record = latest_record(&app, window);
    assert!(!record.damage_full, "插入子树不得整帧");
    assert_eq!(record.delta.removed, 0, "插入帧不得报告移除");
    assert!(
        record.dirty_nodes.contains(&panel),
        "新子树必须重建 lane：{:?}",
        record.dirty_nodes
    );
    let extent = painted_extent(&app, window, panel);
    assert!(
        extent.max().y >= px(215.0),
        "新子树 extent 必须含阴影 blur 外溢（盒底 190 + offset 6 + pad 19）：{extent:?}"
    );
    assert_damage_covers_extent(&app, window, &record.damage, extent, "新插入");

    settle(&mut app, window);
}

#[test]
fn removing_a_painted_subtree_stays_incremental_and_reports_delta_removed() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let panel = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(120.0),
            px(90.0),
            px(160.0),
            px(100.0),
        )))
        .paint(shadowed_paint())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("panel");
    let survivor = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(20.0),
            px(20.0),
            px(60.0),
            px(40.0),
        )))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("survivor");
    let bystander = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(20.0),
            px(240.0),
            px(60.0),
            px(40.0),
        )))
        .paint(PaintSpec {
            background: Color::BLACK,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("bystander");
    frame_once(&mut app, window);
    let extent = painted_extent(&app, window, panel);

    app.remove_node(window, panel).expect("remove panel");
    frame_once(&mut app, window);
    let record = latest_record(&app, window);
    assert!(!record.damage_full, "删除子树不得整帧");
    assert_eq!(record.delta.removed, 1, "删除帧必须报告一条 lane 释放");
    assert!(!record.delta.full, "删除帧的 delta 不得退化为 full");
    assert_damage_covers_extent(&app, window, &record.damage, extent, "被删面板旧");

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_absolute_inset(survivor, LogicalPoint::new(px(30.0), px(30.0)));
    let _ = bystander;
    frame_once(&mut app, window);
    let reuse = latest_record(&app, window);
    assert!(!reuse.damage_full, "删除后的局部改动不得整帧");
    assert!(
        reuse.stats.reused_segments > 0,
        "删除之后未变化 lane 的实例段必须继续复用：{:?}",
        reuse.stats
    );
}

#[test]
fn reordering_a_subtree_stays_incremental_and_damages_its_extent() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let bottom = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(100.0),
            px(100.0),
            px(120.0),
            px(80.0),
        )))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("bottom");
    let top = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(140.0),
            px(120.0),
            px(120.0),
            px(80.0),
        )))
        .paint(PaintSpec {
            background: Color::BLACK,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("top");
    frame_once(&mut app, window);
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .get(root)
            .expect("root")
            .children()[1],
        top,
        "前置条件：top 在绘制序末尾"
    );
    let extents: Vec<LogicalRect> = [bottom, top]
        .iter()
        .map(|node| painted_extent(&app, window, *node))
        .collect();
    assert!(
        !extents[0].intersect(extents[1]).is_empty(),
        "前置条件：两个节点必须空间重叠"
    );

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .append_child(root, bottom)
        .expect("reorder bottom to front");
    frame_once(&mut app, window);

    let record = latest_record(&app, window);
    assert!(!record.damage_full, "重排子树不得整帧");
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .get(root)
            .expect("root")
            .children()[1],
        bottom,
        "重排后 bottom 必须在绘制序末尾"
    );
    assert_damage_covers_extent(
        &app,
        window,
        &record.damage,
        extents[0],
        "重排后被提到最上层的节点",
    );
}

#[test]
fn registering_a_scroll_container_is_the_only_structural_full_downgrade() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    frame_once(&mut app, window);

    let viewport = NodeBuilder::boxed()
        .style({
            let mut style = positioned(LogicalRect::new(px(40.0), px(40.0), px(200.0), px(160.0)));
            style.clip = Clip::Bounds;
            style
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("viewport");
    let handle = app
        .make_scroll(window, viewport, ScrollConfig::default())
        .expect("scroll container");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(800.0)))
        .expect("content size");
    let full_frames_before = app
        .window(window)
        .expect("window")
        .tree()
        .paint_counters()
        .full_frames;
    frame_once(&mut app, window);
    let downgraded = latest_record(&app, window);
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .paint_counters()
            .full_frames,
        full_frames_before + 1,
        "滚动容器注册变化必须安全降级为全量绘制遍历"
    );
    assert!(
        !downgraded.damage_full,
        "降级只重建槽位表与受影响 lane，damage 仍必须局部：{:?}",
        downgraded.damage
    );

    let plain = NodeBuilder::boxed()
        .style(positioned(LogicalRect::new(
            px(10.0),
            px(10.0),
            px(40.0),
            px(30.0),
        )))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("plain popup");
    frame_once(&mut app, window);
    let incremental = latest_record(&app, window);
    assert!(
        !incremental.damage_full,
        "普通（无滚动）结构变化必须回到增量：{:?}",
        incremental.damage
    );
    let extent = painted_extent(&app, window, plain);
    assert_damage_covers_extent(&app, window, &incremental.damage, extent, "普通弹层");
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .paint_counters()
            .full_frames,
        full_frames_before + 1,
        "普通结构变化不得再触发全量遍历"
    );

    app.remove_node(window, viewport).expect("remove viewport");
    frame_once(&mut app, window);
    let removed = latest_record(&app, window);
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .paint_counters()
            .full_frames,
        full_frames_before + 2,
        "移除滚动容器（槽位表变化）同样必须降级为全量绘制"
    );
    assert!(
        !removed.damage_full,
        "移除滚动容器的 damage 仍必须局部：{:?}",
        removed.damage
    );
}
#[test]
fn adding_and_removing_scrolled_content_stays_incremental() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let viewport = NodeBuilder::boxed()
        .style({
            let mut style = positioned(LogicalRect::new(px(40.0), px(40.0), px(200.0), px(160.0)));
            style.clip = Clip::Bounds;
            style
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("viewport");
    let handle = app
        .make_scroll(window, viewport, ScrollConfig::default())
        .expect("scroll container");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(800.0)))
        .expect("content size");
    frame_once(&mut app, window);
    let full_frames = app
        .window(window)
        .expect("window")
        .tree()
        .paint_counters()
        .full_frames;

    let row = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(120.0), px(120.0), px(40.0)))
        .paint(shadowed_paint())
        .build(app.window_mut(window).expect("window").tree_mut(), viewport)
        .expect("row");
    frame_once(&mut app, window);
    let inserted = latest_record(&app, window);
    assert!(!inserted.damage_full, "滚动容器内新增子树不得整帧");
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .paint_counters()
            .full_frames,
        full_frames,
        "滚动容器内新增不得触发全量遍历（槽位表不变）"
    );
    let extent = painted_extent(&app, window, row);
    let visible = extent.intersect(LogicalRect::new(px(40.0), px(40.0), px(200.0), px(160.0)));
    assert!(
        !visible.is_empty(),
        "前置条件：新行必须在视口内可见：{extent:?}"
    );
    assert_damage_covers_extent(
        &app,
        window,
        &inserted.damage,
        visible,
        "滚动容器内新行的可见部分",
    );

    app.remove_node(window, row).expect("remove row");
    frame_once(&mut app, window);
    let removed = latest_record(&app, window);
    assert!(!removed.damage_full, "滚动容器内删除子树不得整帧");
    assert_eq!(
        app.window(window)
            .expect("window")
            .tree()
            .paint_counters()
            .full_frames,
        full_frames,
        "滚动容器内删除不得触发全量遍历"
    );
    assert_eq!(removed.delta.removed, 1, "删除行必须报告 lane 释放");
    assert_damage_covers_extent(&app, window, &removed.damage, extent, "滚动容器内旧行");
}

#[test]
fn removing_a_scrolled_container_child_damages_its_screen_position() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let viewport = NodeBuilder::boxed()
        .style({
            let mut style = positioned(LogicalRect::new(px(40.0), px(40.0), px(200.0), px(160.0)));
            style.clip = Clip::Bounds;
            style
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("viewport");
    let handle = app
        .make_scroll(window, viewport, ScrollConfig::default())
        .expect("scroll container");
    app.set_scroll_content(&handle, LogicalSize::new(px(200.0), px(900.0)))
        .expect("content size");
    let row = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(360.0), px(120.0), px(40.0)))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), viewport)
        .expect("row");
    app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(300.0)))
        .expect("scroll");
    frame_once(&mut app, window);
    let row_bounds = app
        .window(window)
        .expect("window")
        .tree()
        .get(row)
        .expect("row")
        .layout()
        .bounds;
    let screen_extent = row_bounds.translate(LogicalPoint::new(px(0.0), px(-300.0)));
    assert!(
        !screen_extent
            .intersect(LogicalRect::new(px(40.0), px(40.0), px(200.0), px(160.0)))
            .is_empty(),
        "前置条件：行必须出现在视口内：{screen_extent:?}"
    );

    app.remove_node(window, row).expect("remove row");
    frame_once(&mut app, window);
    let removed = latest_record(&app, window);
    assert!(!removed.damage_full, "删除滚动容器直接子节点不得整帧");
    assert_eq!(removed.delta.removed, 1, "删除行必须报告 lane 释放");
    assert_damage_covers_extent(
        &app,
        window,
        &removed.damage,
        screen_extent,
        "滚动容器直接子节点的屏幕位置",
    );
}

fn assert_scroll_table(
    app: &App,
    window: WindowId,
    container: NodeId,
    expected_offset: LogicalPoint,
) {
    let target = app.window(window).expect("window");
    let slots = target.scroll_slots();
    assert_eq!(
        slots.len(),
        2,
        "槽位表必须是「哨兵 + 唯一的虚拟列表容器」，不得留下 stale 槽：{slots:?}"
    );
    assert_eq!(slots[0].0, LogicalPoint::ZERO, "哨兵偏移必须为 0");
    assert_eq!(
        slots[1].0, expected_offset,
        "槽位偏移必须等于容器当前的 scroll offset"
    );
    let bounds = target
        .tree()
        .get(container)
        .expect("container")
        .layout()
        .bounds;
    let expected_viewport = target.device_pixel_ratio().to_covering_device_rect(bounds);
    assert_eq!(
        slots[1].1, expected_viewport,
        "槽位视口必须与容器的屏幕裁剪区一致"
    );
}

#[test]
fn a_virtual_list_keeps_its_scroll_table_consistent_across_item_churn_and_popups() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = {
        let mut style = boxed_style(200.0, 120.0);
        style.clip = Clip::Bounds;
        style.position(taffy::style::Position::Relative);
        NodeBuilder::boxed()
            .style(style)
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("virtual container")
    };
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(20.0),
                extents: None,
                overscan: 1,
            },
        )
        .expect("virtual list");
    let build_rows = |_index: usize| {
        NodeBuilder::boxed().layout_override(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(180.0),
            px(20.0),
        ))
    };
    app.set_virtual_items(&list, 100, build_rows)
        .expect("items");
    frame_once(&mut app, window);
    assert_scroll_table(&app, window, container, LogicalPoint::ZERO);

    app.scroll_to(
        &ScrollHandle::new(window, container),
        LogicalPoint::new(px(0.0), px(600.0)),
    )
    .expect("scroll");
    frame_once(&mut app, window);
    let scrolled = latest_record(&app, window);
    assert!(!scrolled.damage_full, "虚拟列表滚动不得整帧");
    assert_eq!(scrolled.delta.scroll, 1, "滚动帧必须携带槽位增量");
    assert_scroll_table(
        &app,
        window,
        container,
        LogicalPoint::new(px(0.0), px(600.0)),
    );

    app.set_virtual_items(&list, 140, build_rows)
        .expect("grow items");
    frame_once(&mut app, window);
    let grown = latest_record(&app, window);
    assert!(!grown.damage_full, "虚拟列表增行不得整帧");
    assert_eq!(
        grown.delta.scroll, 0,
        "增行不得发布滚动表增量（否则说明槽位被错误重映射）"
    );
    assert_scroll_table(
        &app,
        window,
        container,
        LogicalPoint::new(px(0.0), px(600.0)),
    );

    app.set_virtual_items(&list, 80, build_rows)
        .expect("shrink items");
    frame_once(&mut app, window);
    let shrunk = latest_record(&app, window);
    assert!(!shrunk.damage_full, "虚拟列表减行不得整帧");
    assert_eq!(shrunk.delta.scroll, 0, "减行不得发布滚动表增量");
    assert_scroll_table(
        &app,
        window,
        container,
        LogicalPoint::new(px(0.0), px(600.0)),
    );

    let popup = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(160.0), px(120.0), px(80.0)))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("popup");
    frame_once(&mut app, window);
    let opened = latest_record(&app, window);
    assert!(!opened.damage_full, "虚拟列表存在时打开 popup 不得整帧");
    assert_eq!(opened.delta.scroll, 0, "popup 开启不得改变滚动槽位表");
    assert_scroll_table(
        &app,
        window,
        container,
        LogicalPoint::new(px(0.0), px(600.0)),
    );

    app.remove_node(window, popup).expect("close popup");
    frame_once(&mut app, window);
    let closed = latest_record(&app, window);
    assert!(!closed.damage_full, "虚拟列表存在时关闭 popup 不得整帧");
    assert_eq!(closed.delta.scroll, 0, "popup 关闭不得改变滚动槽位表");
    assert_scroll_table(
        &app,
        window,
        container,
        LogicalPoint::new(px(0.0), px(600.0)),
    );
}

#[test]
fn removing_a_child_of_a_nested_scroll_container_damages_its_screen_position() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let outer = NodeBuilder::boxed()
        .style({
            let mut style = positioned(LogicalRect::new(px(40.0), px(40.0), px(200.0), px(160.0)));
            style.clip = Clip::Bounds;
            style
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("outer viewport");
    let outer_scroll = app
        .make_scroll(window, outer, ScrollConfig::default())
        .expect("outer scroll");
    app.set_scroll_content(&outer_scroll, LogicalSize::new(px(200.0), px(900.0)))
        .expect("outer content");
    let inner = NodeBuilder::boxed()
        .style({
            let mut style = positioned(LogicalRect::new(px(0.0), px(300.0), px(200.0), px(160.0)));
            style.clip = Clip::Bounds;
            style
        })
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("inner viewport");
    let inner_scroll = app
        .make_scroll(window, inner, ScrollConfig::default())
        .expect("inner scroll");
    app.set_scroll_content(&inner_scroll, LogicalSize::new(px(200.0), px(800.0)))
        .expect("inner content");
    let row = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(10.0), px(150.0), px(120.0), px(40.0)))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), inner)
        .expect("row");
    app.scroll_to(&outer_scroll, LogicalPoint::new(px(0.0), px(300.0)))
        .expect("outer scroll");
    app.scroll_to(&inner_scroll, LogicalPoint::new(px(0.0), px(50.0)))
        .expect("inner scroll");
    frame_once(&mut app, window);

    let extent = painted_extent(&app, window, row);
    let screen_extent = extent.translate(LogicalPoint::new(px(0.0), px(-350.0)));
    assert_eq!(
        screen_extent,
        LogicalRect::new(px(50.0), px(140.0), px(120.0), px(40.0)),
        "前置条件：行必须落在两层滚动后的屏幕位置"
    );

    app.remove_node(window, row).expect("remove row");
    frame_once(&mut app, window);
    let removed = latest_record(&app, window);
    assert!(!removed.damage_full, "删除嵌套滚动容器的直接子节点不得整帧");
    assert_damage_covers_extent(
        &app,
        window,
        &removed.damage,
        screen_extent,
        "嵌套滚动容器的直接子节点屏幕位置（差的是内层容器自身的 scroll offset）",
    );
}

fn register_triangle_geometry(app: &mut App) -> crate::paint::geometry::GeometryId {
    use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};
    let points = [[0.0f32, 0.0, -1.0], [20.0, 0.0, -1.0], [0.0, 10.0, -1.0]];
    let vertex_bytes: Vec<u8> = points
        .iter()
        .flat_map(|point| point.iter().flat_map(|value| value.to_ne_bytes()))
        .collect();
    app.geometry_mut()
        .insert(GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from_static(&[0u8, 0, 1, 0, 2, 0]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(20.0), px(10.0)),
            pipeline: GeometryPipeline::Ui,
        })
        .expect("注册三角几何")
}

#[test]
fn standalone_geometry_structural_changes_escalate_to_full_damage() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    frame_once(&mut app, window);
    let geometry = register_triangle_geometry(&mut app);
    let node = NodeBuilder::geometry(geometry)
        .layout_override(LogicalRect::new(px(20.0), px(20.0), px(80.0), px(60.0)))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("几何节点");
    frame_once(&mut app, window);
    assert!(
        latest_record(&app, window).damage_full,
        "I6 边界：新增独立 Geometry 节点（首帧布局变化）升级为整帧 damage"
    );

    app.window_mut(window)
        .expect("window")
        .tree_mut()
        .set_layout_override(
            node,
            Some(crate::layout::LayoutOverride::new(LogicalRect::new(
                px(200.0),
                px(120.0),
                px(80.0),
                px(60.0),
            ))),
        )
        .expect("移动几何节点");
    frame_once(&mut app, window);
    assert!(
        latest_record(&app, window).damage_full,
        "I6 边界：独立 Geometry 节点按布局移动仍升级为整帧 damage"
    );

    app.remove_node(window, node).expect("remove geometry node");
    frame_once(&mut app, window);
    assert!(
        latest_record(&app, window).damage_full,
        "I6 边界：删除独立 Geometry 节点仍升级为整帧 damage"
    );
}

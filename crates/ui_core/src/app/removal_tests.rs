//! E16：节点子树卸载的自动清理（R-21 引擎下沉）的引擎级用例。
//!
//! 断言面：
//! - `RemoveOutcome` 的四类清理计数（节点 / scoped 键绑定 / `after_scoped` 定时器 / 动画槽）
//!   与 `remove_node_with_scope` 的作用域回收计数；
//! - 语义边界：祖先删除清理整棵子树、只删后代保留祖先状态、重复删除是强类型 `StaleNode`
//!   （不是静默 no-op）、root / 跨窗口 / 未知窗口错误且**无副作用**；
//! - 不清理面（应用义务）：Action handler 与全局键绑定按 `Action` / 按键全局索引，引擎不碰；
//! - 共享动画的保守归属（部分重绘节点存活 ⇒ 保留并剔除死节点）、滚动动画按 `ScrollHandle`；
//! - 1000 轮「挂载 → 卸载」：信号槽位 / 绑定 / 定时器 / 动画 / 依赖边零增长；
//! - 关窗路径行为不变；I3（清理本身不留下帧请求 / 唤醒源）。
//!
//! 全部走公开 + `pub(crate)` 面（`App::remove_node` 本身是公开 API；`timer_count_for_test` /
//! `has_action_handler` / `scoped_key_binding_count` 是 `test-support` 观测面），因此整包
//! 只在 `--features test-support` 的 lib 测试里编译。

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
    math::{Point, Rect, Scale, Size},
    node::{
        builder::NodeBuilder,
        dirty::DirtyFlags,
        id::NodeId,
        signal::{Signal, Transient},
    },
    paint::style::{Clip, Style, StylePatch},
    window::{FrameOutcome, WindowId, WindowOptions},
};

fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    // 定时器与动画都用引擎时钟推进（`advance_clock` 是唯一时间源）。
    app.use_engine_clock();
    let window = app.create_window(WindowOptions {
        title: "e16".into(),
        size: Size::new(400.0, 300.0),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    (rt, app, window)
}

fn root(app: &App, window: WindowId) -> NodeId {
    app.window(window).expect("window").tree().root()
}

fn boxed_style(width: f32, height: f32) -> Style {
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::length(width),
        height: taffy::style::Dimension::length(height),
    };
    style
}

/// 可聚焦的普通容器（`on_click` 让它进入焦点判定；不注册 handler ⇒ 点击没有副作用）。
fn mount_box(app: &mut App, window: WindowId, parent: NodeId) -> NodeId {
    NodeBuilder::boxed()
        .style(boxed_style(40.0, 40.0))
        .on_click(Action::custom("focusable"))
        .build(app.window_mut(window).expect("window").tree_mut(), parent)
        .expect("mount box")
}

/// 绑定信号的容器（`bind_style` 产生一条节点 → 信号依赖边）。
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

/// 带裁剪的滚动容器（`App::make_scroll` 的前置条件）。
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

/// 一直出帧到 `Idle`（把挂载 / 依赖边产生的帧请求排干净）。
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

// ── 清理计数与后置状态 ────────────────────────────────────────────────────

/// 核心用例：删除子树自动清理 scoped 绑定 / `after_scoped` 定时器 / 动画槽，并给出计数；
/// 祖先与其兄弟不受影响；被取消的公开动画按 `Removed` 入队完成事件。
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

    // 完成事件在下一帧入口派发：被取消的公开动画报告 `Removed`（不是 Finished）。
    app.run_frame_hook();
    assert_eq!(
        completions.borrow().as_slice(),
        &[(cancelled, AnimationEndKind::Removed)],
        "只有被卸载路径取消的动画报告 Removed"
    );

    // 定时器：子树内的不再派发，子树外的照常到期。
    app.advance_clock(Duration::from_millis(600));
    app.run_frame_hook();
    assert_eq!(inside_hits.get(), 0, "被卸载 scope 的定时器不得再回调");
    assert_eq!(outside_hits.get(), 1, "无关定时器不得被误伤");
}

/// 共享动画的保守归属已在上面的用例里断言；这里单独覆盖「重绘节点全部存活 ⇒ 不取消」。
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

/// 滚动动画按 `ScrollHandle` 的节点归属：容器被卸载 ⇒ 同批取消（终值是滚动状态，不是公开动画，
/// 因此不产生完成事件）。
#[test]
fn scroll_animations_are_cancelled_with_their_container() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = mount_scroll(&mut app, window, root);
    let handle = app
        .make_scroll(window, container, ScrollConfig::default())
        .expect("scroll container");
    app.set_scroll_content(&handle, Size::new(100.0, 400.0))
        .expect("content");
    app.scroll_to_animated(&handle, Point::new(0.0, 120.0))
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

// ── 语义边界 ──────────────────────────────────────────────────────────────

/// 引擎内部高频卸载路径（虚拟列表行回收）同样清行上的 scoped 资源：否则滚动会留下永不命中
/// 的绑定、到期回调跑在死行上的定时器、以及给死行标脏的动画（R-21 的引擎内部半边）。
#[test]
fn virtual_list_row_recycling_cleans_scoped_state_of_recycled_rows() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let mut style = boxed_style(200.0, 100.0);
    style.clip = Clip::Bounds;
    style.inner.position = taffy::style::Position::Relative;
    let container = NodeBuilder::boxed()
        .style(style)
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("virtual container");
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: 25.0,
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |_index| {
        NodeBuilder::boxed().layout_override(Rect::new(0.0, 0.0, 200.0, 25.0))
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

    // 滚到远处：第 0 行被回收，行上的 scoped 状态必须同批清除。
    app.scroll_to(
        &ScrollHandle::new(window, container),
        Point::new(0.0, 2000.0),
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

/// 祖先删除清理整棵子树；只删后代保留祖先状态；重复删除是强类型 `StaleNode` 且无副作用。
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

    // 只删后代（leaf）：动画的重绘节点全在 leaf 子树内 ⇒ 同批取消；祖先绑定 / 定时器保留。
    let outcome = app.remove_node(window, leaf).expect("remove leaf");
    assert_eq!(outcome.removed_nodes, 1);
    assert_eq!(outcome.scoped_key_bindings, 0);
    assert_eq!(outcome.timers, 0);
    assert_eq!(outcome.animations, 1, "动画重绘节点被卸载 ⇒ 取消");
    assert_eq!(scoped_count(&app, window, inner), 1);
    assert_eq!(timer_count(&app), 1);
    assert_eq!(animation_count(&app), 0);

    // 重复删除：显式失败（不是静默 no-op），且不改变任何计数。
    assert!(matches!(
        app.remove_node(window, leaf),
        Err(EngineError::StaleNode { node }) if node == leaf
    ));
    assert_eq!(scoped_count(&app, window, inner), 1);
    assert_eq!(timer_count(&app), 1);
    assert_eq!(animation_count(&app), 0);

    // 祖先删除：整棵子树（inner 自身；leaf 已不在）一并清理。先给 inner 注册一条动画，
    // 验证「祖先删除 ⇒ 其自身重绘节点的动画同批取消」。
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
    // 祖先（outer）保留：它自己仍在树里。
    assert!(
        app.window(window)
            .expect("window")
            .tree()
            .get(outer)
            .is_ok(),
        "祖先节点不得被误删"
    );
}

/// 未知窗口 / root / 跨窗口句柄：强类型错误，且**不产生任何副作用**（绑定 / 定时器 / 动画 /
/// 节点树都不变）。
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

// ── 作用域释放（remove_node_with_scope）────────────────────────────────────

/// 一次调用完成「卸载节点 → 释放作用域」：值槽位回收、引用其瞬态值的窗口级动画同批取消并
/// 报告 `Released`；节点先卸载保证 `bind_*` 闭包不会被求值到已回收句柄。
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

    // 窗口级动画（`nodes = []`，不归属任何子树）⇒ 只能由 `release_scope` 路径取消。
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

    // 值已回收：旧句柄读确定性 panic（不会读到槽位复用后的新值）。
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

    // 节点已卸载 ⇒ 重复删除是 StaleNode；已释放的作用域在另一个节点上按幂等处理。
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

// ── 不清理面（应用义务）────────────────────────────────────────────────────

/// `on_action` handler 与全局 `bind_key` 按 `Action` / 按键**全局**索引：无法归属到一棵子树
/// （同一动作可能被多个组件 / 窗口共享），引擎不得自动摘除（最小安全设计）。scoped 绑定则
/// 必须被清掉。
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
    // 焦点已陈旧 ⇒ 按键不得再命中被卸载 scope 的绑定（既不 panic 也不触发 handler）。
    press(&mut app, window, KeyCode::ArrowDown);
    assert_eq!(scoped_hits.get(), 1, "被卸载 scope 的绑定不得再命中");
    assert!(app.off_action(&action), "调用方仍可显式摘除 handler");
}

// ── 泄漏回归：1000 轮 churn ────────────────────────────────────────────────

/// 1000 轮「挂载子树 + 作用域 + 绑定 + 定时器 + 动画 → 一次调用卸载并回收」：
/// 信号槽位峰值 / 空闲栈 / 依赖边 / 绑定 / 定时器 / 动画 / 待派发完成事件都不增长。
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

        // 批次结束：表全部回零；完成事件在下一帧入口派发掉（不留无界队列）。
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

// ── 关窗路径行为不变 + I3 ─────────────────────────────────────────────────

/// 关窗路径沿用既有清理（scoped 绑定 / 定时器 / 动画 / 窗口作用域），且关窗后 `remove_node`
/// 报 `UnknownWindow`（不是 StaleNode）。
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

/// I3：清理本身不留下帧请求 / 定时器截止时刻 / 连续帧源；卸载后一个提交帧就回到 Idle。
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

    // 卸载本身请求一帧（树变了）；之后必须回到空闲 —— 清理不额外请求帧。
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

// ── E16b 审查修复（F1 悬停 / F2 帧请求 / F3 虚拟行 / F4 作用域错配）──────────

/// F2（E16b）：删子树必须请求**一帧**，且静止窗口的下一帧真的提交（`FrameIndex + 1`）——
/// 漏掉这一步画面会停在旧像素直到下一次无关事件（审查变异 M7 曾全绿；本用例必红）。
#[test]
fn removing_a_subtree_requests_the_next_frame() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let container = mount_box(&mut app, window, root);
    let _leaf = mount_box(&mut app, window, container);

    // 先出到静止，记下最后一次提交的序号。
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

/// F1（E16b）：删除「指针正悬停的子树」必须收尾悬停 / 按压 / 命中并派生 `HoverChanged` ——
/// `update_hover` 的 `previous` 会把已卸载节点过滤成 `None`，删后补调 `revalidate_hover` 会
/// 早退，应用侧 tooltip / `:hover` 会残留到下一次指针移动（审查探针 P1）。
#[test]
fn removing_a_hovered_subtree_derives_a_leave_event() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    // 存活祖先挂 `bind_style`：悬停链变化必须让它重算 `:hover`。
    let keep = NodeBuilder::boxed()
        .style(boxed_style(120.0, 120.0))
        .bind_style(|_| StylePatch::default())
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount keep");
    let outer = mount_box(&mut app, window, keep);
    let inner = NodeBuilder::boxed()
        .layout_override(Rect::new(10.0, 10.0, 20.0, 20.0))
        .build(app.window_mut(window).expect("window").tree_mut(), outer)
        .expect("mount inner");
    settle(&mut app, window);
    let _ = app.drain_events();

    // 悬停 + 按下（未越过拖拽阈值 ⇒ 无捕获）：press / last_hit 都指向 inner。
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(20.0, 20.0), Modifiers::empty()),
    );
    app.dispatch_input(
        window,
        InputEvent::pointer_down(
            Point::new(20.0, 20.0),
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

    // 无关悬停不被打扰：悬停另一个节点后删除不相干子树 ⇒ 不派生 HoverChanged。
    let other = NodeBuilder::boxed()
        .layout_override(Rect::new(250.0, 10.0, 40.0, 40.0))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("mount other");
    settle(&mut app, window);
    let _ = app.drain_events();
    app.dispatch_input(
        window,
        InputEvent::pointer_move(Point::new(260.0, 20.0), Modifiers::empty()),
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

/// F3（E16b）：已挂载的虚拟行只能由引擎回收 —— 应用直接删行在 debug 构建里 `debug_assert`
/// 拒绝且零副作用；删除容器本身仍合法（行随容器一起消失）。
#[test]
fn removing_a_mounted_virtual_row_is_rejected_without_side_effects() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    let mut style = boxed_style(200.0, 100.0);
    style.clip = Clip::Bounds;
    style.inner.position = taffy::style::Position::Relative;
    let container = NodeBuilder::boxed()
        .style(style)
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("virtual container");
    let list = app
        .make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: 25.0,
                extents: None,
                overscan: 0,
            },
        )
        .expect("virtual list");
    app.set_virtual_items(&list, 100, |_index| {
        NodeBuilder::boxed().layout_override(Rect::new(0.0, 0.0, 200.0, 25.0))
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

    // 拒绝必须无副作用：行还在、`mounted` 索引原样。
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

    // 删除容器本身仍合法：行随容器一起消失，虚拟槽登记一并清理。
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

/// F4（E16b）：`SignalScope` 与 `NodeId` 正交 ⇒ 引擎只能拒绝**可证明**的错配：作用域的存活
/// 信号若仍被子树外的节点读取，释放必然打断该绑定 ⇒ `ScopeOwnershipMismatch` 且零副作用。
/// 没有存活读者的错配无法识别（文档契约）；错误面（StaleNode）不得提前释放作用域。
#[test]
fn remove_node_with_scope_rejects_provable_scope_mismatch() {
    let (_rt, mut app, window) = setup();
    let root = root(&app, window);
    // 组件 A：存活节点 + 绑定的作用域信号（依赖边来自 `refresh_styles`）。
    let scope_a = app.signal_scope();
    let signal_a: Signal<u32> = app.signal_scoped(scope_a, 7);
    let node_a = mount_bound(&mut app, window, root, signal_a);
    app.refresh_styles(window).expect("window");
    assert_eq!(app.dependency_edge_count_for_test(), 1);
    // 组件 B：准备卸载的无关子树。
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
    // 零副作用：节点未删、值未释放、依赖边与槽位不变。
    assert_eq!(app.window(window).expect("window").tree().len(), tree_len);
    assert_eq!(*app.read(&signal_a), 7, "错误路径不得释放作用域");
    assert_eq!(app.dependency_edge_count_for_test(), 1);
    assert_eq!(app.store.live_slot_count_for_test(), live_slots);

    // 正确归属（读者全在被删子树内）⇒ 照常释放。
    let scope_b = app.signal_scope();
    let bound_signal: Signal<u32> = app.signal_scoped(scope_b, 1);
    let node_c = mount_bound(&mut app, window, root, bound_signal);
    app.refresh_styles(window).expect("window");
    let outcome = app
        .remove_node_with_scope(window, node_c, scope_b)
        .expect("正确归属的作用域");
    assert_eq!(outcome.released_values, 1);

    // 错误面仍先于清理：`StaleNode` 不得提前释放活跃作用域。
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

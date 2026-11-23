use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use crate::{
    app::{AnimationCompletion, AnimationEndKind, App, TimerId},
    interaction::{AnimatedValue, AnimationSpec, Easing},
    math::Rect,
    node::builder::NodeBuilder,
    paint::{
        color::{Color, ColorResolver},
        style::{PaintSpec, StylePatch},
    },
    test_support::app::TestApp,
    window::{FrameOutcome, WindowId, WindowOptions},
};

fn pump_round(app: &mut TestApp) -> bool {
    let _pump_wake = app.app().take_pump_wake_for_test();
    app.app_mut().run_frame_hook();
    app.drain().expect("frame must not fail").rendered
}

fn painted_node(app: &mut TestApp, background: Color) -> crate::node::id::NodeId {
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .paint(PaintSpec {
            background,
            ..PaintSpec::default()
        })
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            root,
        )
        .expect("mount painted node")
}

fn animate_scalar(
    app: &mut TestApp,
    target: crate::node::signal::Transient<f32>,
    nodes: &[crate::node::id::NodeId],
    to: f32,
    millis: u64,
) -> crate::interaction::AnimationId {
    let window = app.window_id();
    app.app_mut()
        .animate(
            window,
            AnimatedValue::Scalar { target, to },
            nodes,
            AnimationSpec {
                duration: Duration::from_millis(millis),
                easing: Easing::Linear,
            },
        )
        .expect("animate")
}

fn completion_log(app: &mut TestApp) -> Rc<RefCell<Vec<AnimationCompletion>>> {
    let log: Rc<RefCell<Vec<AnimationCompletion>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&log);
    app.app_mut()
        .on_animation_complete(move |_app, event| sink.borrow_mut().push(event));
    log
}

fn kinds(
    log: &Rc<RefCell<Vec<AnimationCompletion>>>,
) -> Vec<(crate::interaction::AnimationId, AnimationEndKind)> {
    log.borrow()
        .iter()
        .map(|event| (event.id, event.kind))
        .collect()
}

#[test]
fn a_finished_animation_notifies_in_the_same_frame_and_the_next_hook_sees_the_final_value() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([10, 20, 30, 255]));
    let fade = app.app_mut().transient(0.0_f32);
    let log = completion_log(&mut app);

    let hook_values: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&hook_values);
    app.app_mut()
        .on_frame(move |app| observed.borrow_mut().push(*app.read_transient(&fade)));

    let id = animate_scalar(&mut app, fade, &[node], 1.0, 100);

    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app), "动画 tick 必须提交一帧");
    assert!(log.borrow().is_empty(), "半程不得派发完成事件");
    assert_eq!(*app.app().read_transient(&fade), 0.5);

    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app), "终值帧必须提交（节点被标 PAINT）");
    assert_eq!(*app.app().read_transient(&fade), 1.0);
    assert_eq!(kinds(&log), vec![(id, AnimationEndKind::Finished)]);
    assert_eq!(
        app.app().pending_animation_completions_for_test(),
        0,
        "事件在同一帧内派发完毕"
    );

    assert!(
        app.app().take_pump_wake_for_test(),
        "自然完成必须唤醒泵一次，让下一轮 on_frame 钩子看到终值（R01/R02 根治点）"
    );
    assert!(
        !app.app().needs_continuous_frames(),
        "动画结束后不再是连续帧源（I3）"
    );

    let frame_index = app.app().window(window).expect("window").frame_index();
    assert!(!pump_round(&mut app), "收尾钩子轮不得提交空帧");
    assert_eq!(
        app.app().window(window).expect("window").frame_index(),
        frame_index,
        "收尾钩子轮不推进 FrameIndex"
    );
    assert_eq!(
        *hook_values.borrow(),
        vec![0.0, 0.5, 1.0],
        "最后一 tick 之后必须再跑一轮钩子，且看到终值"
    );
    assert_eq!(
        kinds(&log),
        vec![(id, AnimationEndKind::Finished)],
        "完成事件只派发一次"
    );
}

#[test]
fn a_cancelled_animation_reports_cancelled_and_never_reports_finished() {
    let mut app = TestApp::new();
    let node = painted_node(&mut app, Color::from_rgba8([1, 2, 3, 255]));
    let fade = app.app_mut().transient(0.0_f32);
    let log = completion_log(&mut app);
    let id = animate_scalar(&mut app, fade, &[node], 1.0, 100);

    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app));

    assert!(app.app_mut().cancel_animation(&id));
    assert!(
        log.borrow().is_empty(),
        "取消事件不得在调用点同步派发（应用回调重入）"
    );
    assert_eq!(app.app().pending_animation_completions_for_test(), 1);
    assert!(
        app.app().take_pump_wake_for_test(),
        "帧外终止必须唤醒泵一轮派发事件"
    );

    assert!(!pump_round(&mut app), "没有待画内容 ⇒ 空闲帧");
    assert_eq!(kinds(&log), vec![(id, AnimationEndKind::Cancelled)]);

    app.advance(Duration::from_millis(200));
    let _ = pump_round(&mut app);
    assert_eq!(
        kinds(&log),
        vec![(id, AnimationEndKind::Cancelled)],
        "取消后不得再报告 Finished"
    );
}

#[test]
fn a_replaced_animation_reports_replaced_and_the_replacement_still_finishes() {
    let mut app = TestApp::new();
    let node = painted_node(&mut app, Color::from_rgba8([4, 5, 6, 255]));
    let fade = app.app_mut().transient(0.0_f32);
    let log = completion_log(&mut app);

    let first = animate_scalar(&mut app, fade, &[node], 1.0, 100);
    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app));

    let second = animate_scalar(&mut app, fade, &[node], 0.0, 100);
    assert!(log.borrow().is_empty(), "替换事件同样在下一帧入口派发");
    let _ = pump_round(&mut app);
    assert_eq!(
        kinds(&log),
        vec![(first, AnimationEndKind::Replaced)],
        "旧动画只报告 Replaced"
    );

    app.advance(Duration::from_millis(100));
    let _ = pump_round(&mut app);
    assert_eq!(
        kinds(&log),
        vec![
            (first, AnimationEndKind::Replaced),
            (second, AnimationEndKind::Finished)
        ],
        "新动画正常完成；旧动画不得再报告 Finished"
    );
    assert_eq!(*app.app().read_transient(&fade), 0.0);
}

#[test]
fn an_animation_whose_scope_is_released_reports_released() {
    let mut app = TestApp::new();
    let node = painted_node(&mut app, Color::from_rgba8([7, 8, 9, 255]));
    let scope = app.app_mut().signal_scope();
    let fade = app.app_mut().transient_scoped(scope, 0.0_f32);
    let log = completion_log(&mut app);
    let id = animate_scalar(&mut app, fade, &[node], 1.0, 100);

    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app));

    app.app_mut().release_scope(scope);
    let _ = pump_round(&mut app);
    assert_eq!(kinds(&log), vec![(id, AnimationEndKind::Released)]);

    app.advance(Duration::from_millis(200));
    let _ = pump_round(&mut app);
    assert_eq!(
        kinds(&log),
        vec![(id, AnimationEndKind::Released)],
        "释放后不得再报告 Finished（也不得写已回收的瞬态槽）"
    );
}

#[test]
fn a_zero_duration_animation_reports_finished_before_the_next_frame() {
    let mut app = TestApp::new();
    let node = painted_node(&mut app, Color::from_rgba8([11, 12, 13, 255]));
    let fade = app.app_mut().transient(0.0_f32);
    let log = completion_log(&mut app);
    let id = animate_scalar(&mut app, fade, &[node], 1.0, 0);

    assert_eq!(*app.app().read_transient(&fade), 1.0, "零时长立即到位");
    assert!(log.borrow().is_empty(), "事件不在此刻同步派发");
    assert_eq!(app.app().pending_animation_completions_for_test(), 1);
    let _ = pump_round(&mut app);
    assert_eq!(kinds(&log), vec![(id, AnimationEndKind::Finished)]);
}

#[test]
fn a_completion_callback_can_recolor_the_node_in_the_same_frame() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let background = app.app_mut().signal(Color::from_rgba8([0, 0, 0, 255]));
    let source = background;
    let node = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
        .bind_style(move |cx| StylePatch {
            background: Some(*cx.read(&source)),
            ..StylePatch::default()
        })
        .build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            root,
        )
        .expect("mount bound node");
    let fade = app.app_mut().transient(0.0_f32);
    let recolor = background;
    app.app_mut().on_animation_complete(move |app, event| {
        if event.kind == AnimationEndKind::Finished {
            app.write(&recolor, Color::from_rgba8([255, 255, 255, 255]));
        }
    });
    let _id = animate_scalar(&mut app, fade, &[node], 1.0, 40);

    app.advance(Duration::from_millis(40));
    assert!(pump_round(&mut app), "完成帧必须提交");
    let rendered = app.rendered().expect("committed frame");
    assert!(
        rendered
            .quads
            .iter()
            .any(|quad| quad.fill == ColorResolver.resolve(Color::WHITE)),
        "完成回调里写的颜色必须参与**本帧**绘制：{:?}",
        rendered.quads
    );
}

#[test]
fn closing_a_window_drops_its_pending_completion_events() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([14, 15, 16, 255]));
    let fade = app.app_mut().transient(0.0_f32);
    let log = completion_log(&mut app);
    let id = animate_scalar(&mut app, fade, &[node], 1.0, 100);
    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app));

    assert!(app.app_mut().cancel_animation(&id));
    assert_eq!(app.app().pending_animation_completions_for_test(), 1);
    assert!(app.app_mut().close_window(window));
    assert_eq!(
        app.app().pending_animation_completions_for_test(),
        0,
        "关窗必须丢弃该窗口的待派发完成事件"
    );
    app.advance(Duration::from_millis(200));
    app.app_mut().run_frame_hook();
    assert!(log.borrow().is_empty(), "关窗后不得再派发该窗口的事件");
}

#[test]
fn a_cancelled_animation_is_dispatched_at_the_next_frame_entry_without_a_pump_hook() {
    let mut app = TestApp::new();
    let node = painted_node(&mut app, Color::from_rgba8([17, 18, 19, 255]));
    let fade = app.app_mut().transient(0.0_f32);
    let log = completion_log(&mut app);
    let id = animate_scalar(&mut app, fade, &[node], 1.0, 100);
    app.advance(Duration::from_millis(50));
    let _ = app.drain().expect("animation frame");
    assert!(app.app_mut().cancel_animation(&id));
    let _ = app.drain().expect("headless frame");
    assert_eq!(kinds(&log), vec![(id, AnimationEndKind::Cancelled)]);
}

#[test]
fn a_completion_callback_can_register_a_new_animation_that_runs_to_completion() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([43, 44, 45, 255]));
    let first_value = app.app_mut().transient(0.0_f32);
    let second_value = app.app_mut().transient(0.0_f32);
    let log: Rc<RefCell<Vec<AnimationCompletion>>> = Rc::new(RefCell::new(Vec::new()));
    let nested: Rc<Cell<Option<crate::interaction::AnimationId>>> = Rc::new(Cell::new(None));

    let first = animate_scalar(&mut app, first_value, &[node], 1.0, 100);
    {
        let sink = Rc::clone(&log);
        let nested = Rc::clone(&nested);
        app.app_mut().on_animation_complete(move |app, event| {
            sink.borrow_mut().push(event);
            if event.kind == AnimationEndKind::Finished && event.id == first {
                let id = app
                    .animate(
                        window,
                        AnimatedValue::Scalar {
                            target: second_value,
                            to: 1.0,
                        },
                        &[node],
                        AnimationSpec {
                            duration: Duration::from_millis(40),
                            easing: Easing::Linear,
                        },
                    )
                    .expect("回调里再注册动画");
                nested.set(Some(id));
            }
        });
    }

    app.advance(Duration::from_millis(100));
    assert!(pump_round(&mut app), "第一条动画的终值帧必须提交");
    assert_eq!(kinds(&log), vec![(first, AnimationEndKind::Finished)]);
    let second = nested
        .get()
        .expect("回调里注册的动画必须真的进入动画表（M4 变异防线）");
    assert_ne!(second, first);

    app.advance(Duration::from_millis(40));
    let _ = pump_round(&mut app);
    assert_eq!(
        kinds(&log),
        vec![
            (first, AnimationEndKind::Finished),
            (second, AnimationEndKind::Finished)
        ],
        "回调里注册的动画必须真的跑完并报告 Finished（收尾覆盖动画表时在这里变红）"
    );
    assert_eq!(
        *app.app().read_transient(&second_value),
        1.0,
        "嵌套动画的终值必须写入"
    );
    assert!(!app.app().animations_running(window));
    assert_eq!(app.app().pending_animation_completions_for_test(), 0);
}

#[test]
fn a_completion_callback_can_cancel_another_animation_in_the_same_dispatch() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([46, 47, 48, 255]));
    let finishing_value = app.app_mut().transient(0.0_f32);
    let victim_value = app.app_mut().transient(0.0_f32);
    let log: Rc<RefCell<Vec<AnimationCompletion>>> = Rc::new(RefCell::new(Vec::new()));
    let cancelled = Rc::new(Cell::new(false));

    let finishing = animate_scalar(&mut app, finishing_value, &[node], 1.0, 40);
    let victim = animate_scalar(&mut app, victim_value, &[node], 1.0, 500);
    {
        let sink = Rc::clone(&log);
        let cancelled = Rc::clone(&cancelled);
        app.app_mut().on_animation_complete(move |app, event| {
            sink.borrow_mut().push(event);
            if event.kind == AnimationEndKind::Finished && event.id == finishing {
                cancelled.set(app.cancel_animation(&victim));
            }
        });
    }

    app.advance(Duration::from_millis(40));
    let _ = pump_round(&mut app);
    assert!(
        cancelled.get(),
        "回调里的 cancel_animation 必须命中另一条动画"
    );
    assert_eq!(
        kinds(&log),
        vec![
            (finishing, AnimationEndKind::Finished),
            (victim, AnimationEndKind::Cancelled)
        ],
        "Cancelled 必须由同一派发循环在本轮送达（外层循环继续消费重入产出的事件）"
    );
    assert_eq!(app.app().pending_animation_completions_for_test(), 0);
    assert!(!app.app().animations_running(window));
    let victim_value = *app.app().read_transient(&victim_value);
    assert!(
        (victim_value - 0.08).abs() < 1e-5,
        "取消发生在本帧 tick 之后：只保留 tick 已写入的中间值 0.08，不得推进到终值 1.0，实际 {victim_value}"
    );
}

#[test]
fn a_completion_callback_can_close_the_window_without_panicking() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([49, 50, 51, 255]));
    let value = app.app_mut().transient(0.0_f32);
    let closed = Rc::new(Cell::new(false));
    let id = animate_scalar(&mut app, value, &[node], 1.0, 40);
    {
        let closed = Rc::clone(&closed);
        app.app_mut().on_animation_complete(move |app, event| {
            if event.kind == AnimationEndKind::Finished && event.id == id {
                closed.set(app.close_window(window));
            }
        });
    }

    app.advance(Duration::from_millis(40));
    let outcome = app.app_mut().frame(window).expect("帧入口时窗口仍然存在");
    assert!(
        matches!(outcome, Ok(FrameOutcome::Idle)),
        "回调里关窗后本帧必须安全返回 Idle（不 panic、不提交像素）"
    );
    assert!(closed.get(), "回调里的 close_window 必须成功");
    assert!(app.app().window(window).is_none(), "窗口必须已关闭");
    assert_eq!(
        app.app().pending_animation_completions_for_test(),
        0,
        "关窗必须丢弃该窗口尚未派发的完成事件"
    );
    assert!(!app.app().animations_running(window));
}

#[test]
fn a_reentrant_dispatch_completions_call_keeps_the_outer_batch_alive() {
    let mut app = TestApp::new();
    let node = painted_node(&mut app, Color::from_rgba8([52, 53, 54, 255]));
    let short_value = app.app_mut().transient(0.0_f32);
    let long_value = app.app_mut().transient(0.0_f32);
    let log: Rc<RefCell<Vec<AnimationCompletion>>> = Rc::new(RefCell::new(Vec::new()));

    let short = animate_scalar(&mut app, short_value, &[node], 1.0, 40);
    let long = animate_scalar(&mut app, long_value, &[node], 1.0, 500);
    {
        let sink = Rc::clone(&log);
        app.app_mut().on_animation_complete(move |app, event| {
            sink.borrow_mut().push(event);
            if event.kind == AnimationEndKind::Finished && event.id == short {
                assert!(app.cancel_animation(&long), "回调里取消另一条动画");
                super::animation::dispatch_completions(app);
            }
        });
    }

    app.advance(Duration::from_millis(40));
    let _ = pump_round(&mut app);
    assert_eq!(
        kinds(&log),
        vec![
            (short, AnimationEndKind::Finished),
            (long, AnimationEndKind::Cancelled)
        ],
        "重入派发不得静默丢光当批事件（删除重入守卫时在这里变红）"
    );
    assert_eq!(app.app().pending_animation_completions_for_test(), 0);
}

#[test]
fn a_timer_fires_exactly_once_at_its_deadline() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let _ = pump_round(&mut app);
    let _ = app.app().take_pump_wake_for_test();
    let hits = Rc::new(Cell::new(0u32));
    let sink = Rc::clone(&hits);
    let id: TimerId = app
        .app_mut()
        .after(window, Duration::from_millis(500), move |_app| {
            sink.set(sink.get() + 1);
        })
        .expect("timer");
    let clock = app.app().clock();
    let due = clock + Duration::from_millis(500);
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(due),
        "平台泵必须把等待截止到「登记时的引擎时钟 + delay」（精确 due 时刻）"
    );
    assert_eq!(app.app().timer_count_for_test(), 1);
    assert!(
        !app.app().take_pump_wake_for_test(),
        "注册定时器不得唤醒泵（等待期零唤醒）"
    );
    let requests = app.app().frame_requests();

    for _ in 0..31 {
        app.advance(Duration::from_millis(16));
        assert!(!pump_round(&mut app), "等待期间不得出帧");
    }
    assert_eq!(hits.get(), 0, "496 ms 内不得触发（不早于 delay）");
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(due),
        "等待期截止时刻不漂移（仍是登记时的 clock + delay）"
    );
    assert_eq!(
        app.app().frame_requests(),
        requests,
        "等待期间不请求帧（I3）"
    );
    assert!(
        !app.app().needs_continuous_frames(),
        "定时器不是连续帧源（不产生动画式 tick）"
    );
    assert!(
        !app.app().take_pump_wake_for_test(),
        "等待期间零唤醒（到点前平台保持空闲阻塞）"
    );

    app.advance(Duration::from_millis(4));
    let _ = pump_round(&mut app);
    assert_eq!(hits.get(), 1, "到期必须恰好派发一次");
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(
        app.app().next_wake_deadline(),
        None,
        "派发后平台不再挂表（不会周期性醒来）"
    );

    app.advance(Duration::from_secs(1));
    let _ = pump_round(&mut app);
    assert_eq!(hits.get(), 1, "一次性定时器不得重复派发");
    let _ = id;
}

#[test]
fn cancelling_a_timer_is_idempotent_and_prevents_the_callback() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let hits = Rc::new(Cell::new(0u32));
    let sink = Rc::clone(&hits);
    let id = app
        .app_mut()
        .after(window, Duration::from_millis(500), move |_app| {
            sink.set(sink.get() + 1);
        })
        .expect("timer");
    assert!(app.app_mut().cancel_timer(&id));
    assert!(!app.app_mut().cancel_timer(&id), "重复取消失败（幂等）");
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);

    app.advance(Duration::from_secs(1));
    let _ = pump_round(&mut app);
    assert_eq!(hits.get(), 0, "已取消的定时器不得执行回调");
}

#[test]
fn closing_a_window_cancels_its_timers() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([20, 21, 22, 255]));
    let hits = Rc::new(Cell::new(0u32));
    let sink = Rc::clone(&hits);
    let id = app
        .app_mut()
        .after_scoped(window, node, Duration::from_millis(500), move |_app| {
            sink.set(sink.get() + 1);
        })
        .expect("timer");
    let hits_plain = Rc::new(Cell::new(0u32));
    let sink_plain = Rc::clone(&hits_plain);
    let _plain = app
        .app_mut()
        .after(window, Duration::from_millis(500), move |_app| {
            sink_plain.set(sink_plain.get() + 1);
        })
        .expect("timer");
    assert_eq!(app.app().timer_count_for_test(), 2);

    assert!(app.app_mut().close_window(window));
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);
    assert!(!app.app_mut().cancel_timer(&id), "关窗后旧句柄不可再取消");

    app.advance(Duration::from_secs(1));
    app.app_mut().run_frame_hook();
    assert_eq!(hits.get(), 0);
    assert_eq!(hits_plain.get(), 0);
}

#[test]
fn unloading_the_scope_node_cancels_the_timer() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let scope_node = painted_node(&mut app, Color::from_rgba8([23, 24, 25, 255]));
    let unrelated = painted_node(&mut app, Color::from_rgba8([26, 27, 28, 255]));
    let hits = Rc::new(Cell::new(0u32));
    let sink = Rc::clone(&hits);
    app.app_mut()
        .after_scoped(
            window,
            scope_node,
            Duration::from_millis(500),
            move |_app| sink.set(sink.get() + 1),
        )
        .expect("timer");

    assert!(app.app_mut().remove_node(window, unrelated).is_ok());
    assert_eq!(
        app.app().timer_count_for_test(),
        1,
        "卸载无关节点不得误伤定时器"
    );

    assert!(app.app_mut().remove_node(window, scope_node).is_ok());
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);
    app.advance(Duration::from_secs(1));
    let _ = pump_round(&mut app);
    assert_eq!(hits.get(), 0, "scope 卸载后回调不得执行");
}

#[test]
fn timers_are_validated_against_windows_and_scope_nodes() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let unknown = WindowId::for_test(4242);
    match app
        .app_mut()
        .after(unknown, Duration::from_millis(1), |_app| {})
    {
        Err(crate::error::EngineError::UnknownWindow { window: reported }) => {
            assert_eq!(reported, unknown);
        }
        other => panic!("未知窗口必须是 UnknownWindow，实际 {other:?}"),
    }

    let stale = painted_node(&mut app, Color::from_rgba8([29, 30, 31, 255]));
    assert!(app.app_mut().remove_node(window, stale).is_ok());
    match app
        .app_mut()
        .after_scoped(window, stale, Duration::from_millis(1), |_app| {})
    {
        Err(crate::error::EngineError::StaleNode { node }) => assert_eq!(node, stale),
        other => panic!("陈旧 scope 必须是 StaleNode，实际 {other:?}"),
    }
    assert_eq!(app.app().timer_count_for_test(), 0, "校验失败不得留下条目");
}

#[test]
fn the_next_wake_deadline_tracks_the_earliest_timer() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let clock = app.app().clock();
    let slow = app
        .app_mut()
        .after(window, Duration::from_millis(300), |_app| {})
        .expect("slow");
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(clock + Duration::from_millis(300))
    );
    let fast = app
        .app_mut()
        .after(window, Duration::from_millis(100), |_app| {})
        .expect("fast");
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(clock + Duration::from_millis(100)),
        "必须取最早到期时刻（平台只挂一次唤醒）"
    );
    assert!(app.app_mut().cancel_timer(&fast));
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(clock + Duration::from_millis(300)),
        "取消最早的一条后回退到下一条"
    );
    app.advance(Duration::from_millis(300));
    let _ = pump_round(&mut app);
    assert_eq!(app.app().next_wake_deadline(), None);
    let _ = slow;
}

#[test]
fn a_timer_registered_inside_a_callback_never_recurses_within_one_dispatch() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let clock = app.app().clock();
    let outer_hits = Rc::new(Cell::new(0u32));
    let nested_hits = Rc::new(Cell::new(0u32));
    let outer_sink = Rc::clone(&outer_hits);
    let nested_sink = Rc::clone(&nested_hits);
    app.app_mut()
        .after(window, Duration::ZERO, move |app| {
            outer_sink.set(outer_sink.get() + 1);
            let inner = Rc::clone(&nested_sink);
            app.after(window, Duration::ZERO, move |_app| {
                inner.set(inner.get() + 1);
            })
            .expect("回调里注册的 0 ms 定时器");
        })
        .expect("外层 0 ms 定时器");
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(clock),
        "0 ms 的到期时刻 = 登记时的引擎时钟（不延后、不加 ε）"
    );
    assert_eq!(app.app().timer_count_for_test(), 1);

    app.app_mut().run_frame_hook();
    assert_eq!(outer_hits.get(), 1, "外层定时器恰好派发一次");
    assert_eq!(
        nested_hits.get(),
        0,
        "同一次派发调用内不得内联派发回调里注册的 0 ms 定时器（快照语义），实际 {}",
        nested_hits.get()
    );
    assert_eq!(
        app.app().timer_count_for_test(),
        1,
        "内层条目仍在表里，等待下一个派发点"
    );
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(clock),
        "内层 0 ms 的截止时刻仍是引擎时钟（等待期不漂移）"
    );

    let _ = app.drain().expect("frame");
    assert_eq!(nested_hits.get(), 1, "内层定时器必须恰好派发一次");
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);

    app.app_mut().run_frame_hook();
    app.advance(Duration::from_secs(1));
    app.app_mut().run_frame_hook();
    assert_eq!(outer_hits.get(), 1, "外层不得重复派发");
    assert_eq!(nested_hits.get(), 1, "内层不得重复派发");
}

fn renew_zero_delay(app: &mut App, window: WindowId, hits: Rc<Cell<u32>>, steps: u32) {
    hits.set(hits.get() + 1);
    if hits.get() < steps {
        app.after(window, Duration::ZERO, move |app| {
            renew_zero_delay(app, window, hits, steps);
        })
        .expect("零延时自续期");
    }
}

#[test]
fn a_zero_delay_self_renewal_chain_advances_one_step_per_dispatch_and_stays_bounded() {
    const STEPS: u32 = 6;
    let mut app = TestApp::new();
    let window = app.window_id();
    let clock = app.app().clock();
    let hits = Rc::new(Cell::new(0u32));
    let sink = Rc::clone(&hits);
    app.app_mut()
        .after(window, Duration::ZERO, move |app| {
            renew_zero_delay(app, window, sink, STEPS)
        })
        .expect("链头定时器");
    assert_eq!(app.app().next_wake_deadline(), Some(clock));
    let requests = app.app().frame_requests();
    let _ = app.app().take_pump_wake_for_test();

    for step in 1..=STEPS {
        app.app_mut().run_frame_hook();
        assert_eq!(
            hits.get(),
            step,
            "每个派发调用恰好推进一层 0 ms 链（内联重派发会在这里多跳）"
        );
        if step < STEPS {
            assert_eq!(
                app.app().timer_count_for_test(),
                1,
                "链上恒定 1 条待派发（无界增长的防线）"
            );
            assert_eq!(
                app.app().next_wake_deadline(),
                Some(clock),
                "每层续期的截止时刻都精确等于引擎时钟"
            );
        } else {
            assert_eq!(app.app().timer_count_for_test(), 0, "链尾不再挂表");
            assert_eq!(app.app().next_wake_deadline(), None);
        }
        assert!(
            !app.app().take_pump_wake_for_test(),
            "定时器派发不产生帧唤醒位（I3）"
        );
        assert_eq!(
            app.app().frame_requests(),
            requests,
            "自续期链不请求帧（不空转）"
        );
        assert!(!app.app().needs_continuous_frames(), "定时器不是连续帧源");
    }

    app.app_mut().run_frame_hook();
    app.advance(Duration::from_secs(1));
    app.app_mut().run_frame_hook();
    assert_eq!(hits.get(), STEPS, "链只跑登记的步数");
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);
    assert_eq!(app.app().frame_requests(), requests);
}

#[test]
fn a_thousand_churn_rounds_leave_no_timer_or_completion_state_behind() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let fade = app.app_mut().transient(0.0_f32);

    for _ in 0..1000 {
        let id = app
            .app_mut()
            .after(window, Duration::from_millis(5), |_app| {})
            .expect("timer");
        assert_eq!(app.app().timer_count_for_test(), 1);
        assert!(app.app_mut().cancel_timer(&id));
        assert_eq!(app.app().timer_count_for_test(), 0);
    }
    assert_eq!(app.app().next_wake_deadline(), None);

    for _ in 0..1000 {
        let id = animate_scalar(&mut app, fade, &[], 1.0, 5);
        assert!(app.app_mut().cancel_animation(&id));
        let _ = pump_round(&mut app);
        assert_eq!(app.app().pending_animation_completions_for_test(), 0);
        assert!(!app.app().animations_running(window));
    }
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);
}

#[test]
fn a_delayed_timer_then_a_transient_fade_drives_a_tooltip_like_sequence() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([40, 41, 42, 255]));
    let opacity = app.app_mut().transient(0.0_f32);
    let started = Rc::new(Cell::new(false));
    let fade_started = Rc::clone(&started);
    let target = opacity;
    app.app_mut()
        .after(window, Duration::from_millis(500), move |app| {
            fade_started.set(true);
            app.animate(
                window,
                AnimatedValue::Scalar { target, to: 1.0 },
                &[node],
                AnimationSpec {
                    duration: Duration::from_millis(150),
                    easing: Easing::EaseOutCubic,
                },
            )
            .expect("fade-in after the delay");
        })
        .expect("tooltip delay timer");

    let mut elapsed = Duration::ZERO;
    let mut fired_after = None;
    for _ in 0..60 {
        app.advance(Duration::from_millis(16));
        elapsed += Duration::from_millis(16);
        let _ = pump_round(&mut app);
        if started.get() && fired_after.is_none() {
            fired_after = Some(elapsed);
        }
        if started.get()
            && !app.app().animations_running(window)
            && elapsed > Duration::from_millis(500)
        {
            break;
        }
    }

    let fired_after = fired_after.expect("500 ms 后定时器必须触发（真实泵序下不再永远等不到）");
    assert!(
        fired_after >= Duration::from_millis(500),
        "不得早于登记延迟触发，实际 {fired_after:?}"
    );
    assert_eq!(
        *app.app().read_transient(&opacity),
        1.0,
        "延迟到点后启动的淡出动画必须真正跑完（终值写入）"
    );
    assert!(!app.app().animations_running(window));
    assert!(!app.app().needs_continuous_frames(), "结束后回到空闲（I3）");
}

#[test]
fn a_timer_registered_on_a_live_clock_is_anchored_to_the_registration_instant() {
    let mut app = TestApp::new();
    let window = app.window_id();
    app.app_mut().sync_clock_live();
    std::thread::sleep(Duration::from_millis(2));
    let registered_at = Instant::now();
    app.app_mut()
        .after(window, Duration::from_millis(400), |_app| {})
        .expect("timer");
    let deadline = app.app().next_wake_deadline().expect("pending timer");
    assert!(
        deadline >= registered_at + Duration::from_millis(400),
        "deadline 必须以登记瞬间为基准（泵起点时钟会把它算早：macOS 真机 398.648 ms < 400 ms）"
    );
}

#[test]
fn a_timer_registered_before_the_first_pump_is_not_early() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions::default());
    std::thread::sleep(Duration::from_millis(50));
    let registered_at = Instant::now();
    let first = app
        .after(window, Duration::from_millis(1500), |_app| {})
        .expect("timer");
    let deadline = app.next_wake_deadline().expect("pending timer");
    assert!(
        deadline >= registered_at + Duration::from_millis(1500),
        "首次泵启动前登记也必须以登记瞬间为基准（E10c-R01；修复前 Windows 真机探针提前 69.58 ms）"
    );

    assert!(app.cancel_timer(&first));
    app.sync_clock_live();
    std::thread::sleep(Duration::from_millis(2));
    let after_pump = Instant::now();
    app.after(window, Duration::from_millis(400), |_app| {})
        .expect("timer");
    let deadline = app.next_wake_deadline().expect("pending timer");
    assert!(
        deadline >= after_pump + Duration::from_millis(400),
        "泵启动后登记同样以登记瞬间为基准"
    );
}

#[test]
fn waiting_for_a_one_shot_timer_produces_no_frames_or_wakeups() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let _node = painted_node(&mut app, Color::from_rgba8([1, 2, 3, 255]));
    let _ = pump_round(&mut app);
    let frame_index = app.app().window(window).expect("window").frame_index();
    let requests = app.app().frame_requests();

    let fired = Rc::new(Cell::new(false));
    let sink = Rc::clone(&fired);
    app.app_mut()
        .after(window, Duration::from_millis(80), move |_app| {
            sink.set(true)
        })
        .expect("timer");
    assert_eq!(app.app().timer_count_for_test(), 1);

    for _ in 0..3 {
        app.advance(Duration::from_millis(10));
        let outcome = app.app_mut().frame(window);
        assert!(
            matches!(outcome, Some(Ok(FrameOutcome::Idle))),
            "等待定时器期间不得提交帧"
        );
        assert_eq!(
            app.app().window(window).expect("window").frame_index(),
            frame_index,
            "等待期不得推进 FrameIndex"
        );
        assert_eq!(
            app.app().frame_requests(),
            requests,
            "等待定时器不得请求帧（I3）"
        );
        assert!(!fired.get(), "未到期不得派发定时器");
        assert_eq!(app.app().timer_count_for_test(), 1);
    }

    app.advance(Duration::from_millis(90));
    app.app_mut().run_frame_hook();
    assert!(fired.get(), "到点必须派发");
    assert_eq!(app.app().timer_count_for_test(), 0);
    app.app_mut().run_frame_hook();
    assert_eq!(app.app().timer_count_for_test(), 0, "只派发一次");
}

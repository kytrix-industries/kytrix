//! E10：动画完成回调 + 一次性定时器的引擎级用例。
//!
//! 这些用例按**真实泵序**推进（`TestApp::advance` 推进引擎时钟 → `App::run_frame_hook`
//! （平台泵每轮的钩子步骤：派发到期定时器 / 动画终止事件 → 应用钩子）→ `App::frame`），
//! 而不是 P2-4b 审查 R11 批评的「drain 完再手动 sync」写法。completion 的「同帧内」与
//! 「下一帧开始前」两种时序分别由 tick 路径与帧入口路径覆盖。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use crate::{
    app::{AnimationCompletion, AnimationEndKind, TimerId},
    interaction::{AnimatedValue, AnimationSpec, Easing},
    math::Rect,
    node::builder::NodeBuilder,
    paint::{
        color::{Color, ColorResolver},
        style::{PaintSpec, StylePatch},
    },
    test_support::app::TestApp,
    window::WindowId,
};

/// 一轮真实泵的**测试投影**：消费一次「有一帧要画」唤醒位（平台泵的 `take_frame_wake`）→
/// 钩子步骤（按时序派发到期定时器 / 动画终止事件 + 应用 `on_frame`）→ 帧。
///
/// 返回本帧是否**提交了像素**（`FrameRecord::rendered`）。
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

// ── 动画完成回调 ──────────────────────────────────────────────────────────

/// **R01/R02 的核心回归**：自然完成的回调在**写入终值的同一帧内**派发（早于样式/布局/绘制），
/// 且引擎在完成时唤醒泵一次 —— 下一轮 `on_frame` 钩子一定能看到终值，不需要轮询
/// `animations_running`，也不会提交一个空的「收尾帧」。
#[test]
fn a_finished_animation_notifies_in_the_same_frame_and_the_next_hook_sees_the_final_value() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let node = painted_node(&mut app, Color::from_rgba8([10, 20, 30, 255]));
    let fade = app.app_mut().transient(0.0_f32);
    let log = completion_log(&mut app);

    // 钩子观察值：证明「最后一 tick 之后还有一轮钩子」真的跑过。
    let hook_values: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&hook_values);
    app.app_mut()
        .on_frame(move |app| observed.borrow_mut().push(*app.read_transient(&fade)));

    let id = animate_scalar(&mut app, fade, &[node], 1.0, 100);

    // 半程：只有动画帧，没有完成事件。
    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app), "动画 tick 必须提交一帧");
    assert!(log.borrow().is_empty(), "半程不得派发完成事件");
    assert_eq!(*app.app().read_transient(&fade), 0.5);

    // 终值帧：tick 写入 1.0 → 同帧内派发 `Finished`；本帧照常提交（节点被标脏）。
    app.advance(Duration::from_millis(50));
    assert!(pump_round(&mut app), "终值帧必须提交（节点被标 PAINT）");
    assert_eq!(*app.app().read_transient(&fade), 1.0);
    assert_eq!(kinds(&log), vec![(id, AnimationEndKind::Finished)]);
    assert_eq!(
        app.app().pending_animation_completions_for_test(),
        0,
        "事件在同一帧内派发完毕"
    );

    // 完成唤醒：泵被唤醒一次（钩子轮），但不请求帧（`frame_requests` 不因此增长）。
    assert!(
        app.app().take_pump_wake_for_test(),
        "自然完成必须唤醒泵一次，让下一轮 on_frame 钩子看到终值（R01/R02 根治点）"
    );
    assert!(
        !app.app().needs_continuous_frames(),
        "动画结束后不再是连续帧源（I3）"
    );

    // 下一轮：钩子看到终值，帧是空闲帧（不提交像素、FrameIndex 不变）。
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

/// 取消**不**报告 `Finished`；`Cancelled` 事件在下一帧入口派发，调用点不重入应用回调。
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

    // 终值时刻已过也不再补 `Finished`。
    app.advance(Duration::from_millis(200));
    let _ = pump_round(&mut app);
    assert_eq!(
        kinds(&log),
        vec![(id, AnimationEndKind::Cancelled)],
        "取消后不得再报告 Finished"
    );
}

/// 替换：旧动画报告 `Replaced`（**不**是 `Finished`），新动画继续驱动同一目标并能正常完成。
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

/// 目标作用域释放：动画槽被同批取消并报告 `Released`；旧值已回收，后续不会写死槽位。
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

/// 0 时长动画：立即到位 + 报告 `Finished`（下一帧入口派发，不需要 tick）。
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

/// 同帧收尾：完成回调里改的状态**参与本帧绘制**（派发点在样式 / 布局 / 绘制之前）。
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

/// 关窗：该窗口尚未派发的动画终止事件被丢弃（窗口 / 节点已不存在，应用没有可收尾的对象）。
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

/// headless / 自驱帧宿主（没有平台泵钩子）也在**下一帧入口**派发取消事件。
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
    // 不调用 `run_frame_hook`，直接走 `App::frame`（headless 帧宿主）。
    let _ = app.drain().expect("headless frame");
    assert_eq!(kinds(&log), vec![(id, AnimationEndKind::Cancelled)]);
}

// ── 一次性定时器 ──────────────────────────────────────────────────────────

/// 到点**唤醒一次**：等待期零帧 / 零唤醒 / 不进连续帧（I3）；到期回调恰好一次；
/// 派发后 `next_wake_deadline` 清空（平台不再挂表）。
#[test]
fn a_timer_fires_exactly_once_at_its_deadline() {
    let mut app = TestApp::new();
    let window = app.window_id();
    // 先把窗口的初始整帧刷掉：本用例断言的是「定时器等待期」的帧行为（I3）。
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
    assert_eq!(
        app.app().next_wake_deadline(),
        Some(clock + Duration::from_millis(500)),
        "平台泵必须把等待截止到定时器到期时刻"
    );
    assert_eq!(app.app().timer_count_for_test(), 1);
    assert!(
        !app.app().take_pump_wake_for_test(),
        "注册定时器不得唤醒泵（等待期零唤醒）"
    );
    let requests = app.app().frame_requests();

    // 等待期：每 16 ms 一轮泵，全部空闲（不提交像素、不请求帧、不唤醒）。
    for _ in 0..31 {
        app.advance(Duration::from_millis(16));
        assert!(!pump_round(&mut app), "等待期间不得出帧");
    }
    assert_eq!(hits.get(), 0, "499 ms 内不得触发");
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

    // 到点：下一轮泵派发一次。
    app.advance(Duration::from_millis(4)); // 31×16 + 4 = 500 ms
    let _ = pump_round(&mut app);
    assert_eq!(hits.get(), 1, "到期必须恰好派发一次");
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(
        app.app().next_wake_deadline(),
        None,
        "派发后平台不再挂表（不会周期性醒来）"
    );

    // 再推进 1 s：只派发一次（一次性语义）。
    app.advance(Duration::from_secs(1));
    let _ = pump_round(&mut app);
    assert_eq!(hits.get(), 1, "一次性定时器不得重复派发");
    let _ = id;
}

/// 取消：幂等、立即从表里移除、不再安排平台唤醒、回调永不执行。
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

/// 关窗清理：窗口关闭时取消该窗口的全部定时器（含节点作用域的），回调不会跑在死窗口上。
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

/// 节点卸载清理（E8 scoped 语义）：`after_scoped` 的 scope 子树被卸载 ⇒ 自动取消；
/// 卸载**无关**节点不得误伤。
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

    assert!(app.app_mut().remove_node(window, unrelated));
    assert_eq!(
        app.app().timer_count_for_test(),
        1,
        "卸载无关节点不得误伤定时器"
    );

    assert!(app.app_mut().remove_node(window, scope_node));
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);
    app.advance(Duration::from_secs(1));
    let _ = pump_round(&mut app);
    assert_eq!(hits.get(), 0, "scope 卸载后回调不得执行");
}

/// 注册期校验（与 E8 scoped 键绑定同口径）：未知窗口 / 陈旧 scope 节点都是强类型错误。
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
    assert!(app.app_mut().remove_node(window, stale));
    match app
        .app_mut()
        .after_scoped(window, stale, Duration::from_millis(1), |_app| {})
    {
        Err(crate::error::EngineError::StaleNode { node }) => assert_eq!(node, stale),
        other => panic!("陈旧 scope 必须是 StaleNode，实际 {other:?}"),
    }
    assert_eq!(app.app().timer_count_for_test(), 0, "校验失败不得留下条目");
}

/// `next_wake_deadline` 始终指向**最早**的定时器；取消 / 派发后正确回退。
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

/// 回调里注册的新定时器**不会在同一次派发调用内**内联（快照语义）⇒ 0 ms 自我续期不会形成
/// 不返回的循环；它至多在**同一引擎轮次的后半段**（帧入口兜底派发点）或下一轮触发一次。
#[test]
fn a_timer_registered_inside_a_callback_never_recurses_within_one_dispatch() {
    let mut app = TestApp::new();
    let window = app.window_id();
    let nested_hits = Rc::new(Cell::new(0u32));
    let sink = Rc::clone(&nested_hits);
    app.app_mut()
        .after(window, Duration::ZERO, move |app| {
            let sink = Rc::clone(&sink);
            app.after(window, Duration::ZERO, move |_app| sink.set(sink.get() + 1))
                .expect("nested timer");
        })
        .expect("outer timer");

    // 一轮泵至多派发一层新注册的 0 ms 定时器（钩子 → 帧入口两条派发点）：不会无限递归。
    let _ = pump_round(&mut app);
    assert!(
        nested_hits.get() <= 1,
        "同一次引擎轮次至多派发一层新定时器，实际 {}",
        nested_hits.get()
    );
    let after_first_round = nested_hits.get();
    assert_eq!(app.app().timer_count_for_test(), 0);
    let _ = pump_round(&mut app);
    assert_eq!(
        nested_hits.get(),
        after_first_round,
        "一次性定时器不得在后续轮次重复派发"
    );
}

/// 1000 轮 timer / animation churn：表与事件队列不增长（无泄漏），稳态后不残留唤醒计划。
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
        // 每轮走一次泵：取消事件被派发（无 handler ⇒ 丢弃），队列不累积。
        let _ = pump_round(&mut app);
        assert_eq!(app.app().pending_animation_completions_for_test(), 0);
        assert!(!app.app().animations_running(window));
    }
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);
}

/// tooltip 延迟场景的组合回归（P2-4b R02 的真实形态）：500 ms 定时器到点**才**启动
/// 150 ms 淡出动画；全程按 60 Hz 泵序推进，不依赖用户输入、不轮询、不引入连续帧等待期。
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

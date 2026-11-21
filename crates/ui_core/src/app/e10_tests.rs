//! E10：动画完成回调 + 一次性定时器的引擎级用例。
//!
//! 这些用例按**真实泵序**推进（`TestApp::advance` 推进引擎时钟 → `App::run_frame_hook`
//! （平台泵每轮的钩子步骤：派发到期定时器 / 动画终止事件 → 应用钩子）→ `App::frame`），
//! 而不是 P2-4b 审查 R11 批评的「drain 完再手动 sync」写法。completion 的「同帧内」与
//! 「下一帧开始前」两种时序分别由 tick 路径与帧入口路径覆盖。

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

/// 重入①：完成回调里再注册一条动画 —— 新动画必须真的进入动画表、在后续帧正常推进并报告
/// `Finished`（E10 审查 M4 的变异防线：`app.animations = keep` 若挪到派发之后，回调里新注册的
/// 动画会被静默丢弃，本用例在「嵌套动画从不完成」处变红）。
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

/// 重入②：完成回调里取消**另一条**动画 —— `Cancelled` 事件由**同一派发循环**在本轮送达
/// （不是「排队到末尾」也不是丢失），且被取消的动画终值不写。
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

/// 重入③：完成回调里关窗 —— 帧管线中途窗口消失必须安全（不 panic、本帧返回 `Idle`、
/// 未派发事件与定时器一并丢弃）。
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

/// 重入④（R07 选项①）：**直调内层派发**必须被 `dispatching_animation_completions` 守卫挡住，
/// 不得 take 到 `None` 后把外层当批剩余事件清空。当前公开面没有能重入 `dispatch_completions`
/// 的入口（应用回调不能调用帧 / 钩子），因此这里用 crate 内直调模拟未来出现的重入路径——
/// 删除守卫（E10 审查 M8 的存活变异）会让本用例变红。
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
                // 模拟未来的重入派发入口：守卫必须让内层直接返回（外层继续消费 `Cancelled`），
                // 否则内层会走「无 handler ⇒ clear()」把当批剩余事件静默丢光。
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

    // 等待期：每 16 ms 一轮泵，全部空闲（不提交像素、不请求帧、不唤醒）。
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

/// 快照语义（精确断言）：一次 `run_due` 只派发**调用开始时**已到期的条目；回调里注册的 0 ms
/// 定时器**不在同一次调用内**内联，必须等到下一个派发点（`App::frame` 入口兜底或下一轮）。
///
/// 变异防线：把 `run_due` 改成「派发后重新快照、同一次调用里继续派发」时，本用例在
/// 「一次调用恰好 1 次 / 内层尚未派发」处变红。
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

    // 第一个派发点：快照里只有外层 ⇒ 恰好 1 次；内层已登记但**尚未**派发。
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

    // 第二个派发点（`App::frame` 入口兜底）：内层恰好派发一次；表清空、平台不再挂表。
    let _ = app.drain().expect("frame");
    assert_eq!(nested_hits.get(), 1, "内层定时器必须恰好派发一次");
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);

    // 再派发两轮：一次性语义，不重复。
    app.app_mut().run_frame_hook();
    app.advance(Duration::from_secs(1));
    app.app_mut().run_frame_hook();
    assert_eq!(outer_hits.get(), 1, "外层不得重复派发");
    assert_eq!(nested_hits.get(), 1, "内层不得重复派发");
}

/// 0 ms 自续期链的驱动：累计一次命中，未到 `steps` 就在回调里再注册一条 0 ms 定时器。
fn renew_zero_delay(app: &mut App, window: WindowId, hits: Rc<Cell<u32>>, steps: u32) {
    hits.set(hits.get() + 1);
    if hits.get() < steps {
        app.after(window, Duration::ZERO, move |app| {
            renew_zero_delay(app, window, hits, steps);
        })
        .expect("零延时自续期");
    }
}

/// 0 ms 自续期链（A→B→C…）：每次到点再注册一条 0 ms。
///
/// 断言面（E10-R03）：
/// - **每个派发调用恰好推进一层**（同一次调用里不会自旋连跳）；
/// - 链上**恒有 1 条**待派发条目（不会无界增长），结束后归零；
/// - 每次续期的 `next_wake_deadline` 都精确等于**引擎时钟**（不空转、不提前/延后唤醒）；
/// - 定时器派发不置帧唤醒位、不请求帧、不进入连续帧（I3：链由平台一次性唤醒驱动，不产生空帧）。
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

    // 链结束（无续期）后：再派发两轮不重复、不残留。
    app.app_mut().run_frame_hook();
    app.advance(Duration::from_secs(1));
    app.app_mut().run_frame_hook();
    assert_eq!(hits.get(), STEPS, "链只跑登记的步数");
    assert_eq!(app.app().timer_count_for_test(), 0);
    assert_eq!(app.app().next_wake_deadline(), None);
    assert_eq!(app.app().frame_requests(), requests);
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

// ── E10c：登记基准与等待期 I3 ──────────────────────────────────────────────

/// **E10c 的根因回归**：生产（活时钟）下 `App::after` 的 deadline 必须以**登记瞬间**的真实
/// 时间为基准，而不是泵起点同步的引擎时钟。
///
/// macOS 真机失败：泵在轮首同步时钟，之后还有事件/窗口状态/出帧等前置工作；应用在 `on_frame`
/// 里登记 400 ms 定时器时，引擎时钟已经落后真实时间那段工作 ⇒ deadline = 泵起点 + 400 ms，
/// 平台定时器在「登记后 400 ms」之前到点，实测 `398.648 ms < 400 ms`。
///
/// 变异守卫（E10c M1）：把 `timer::after` 的基准改回 `app.clock`（删掉活时钟分支）⇒ 本用例
/// 的 `deadline >= 登记瞬间 + delay` 断言变红（引擎时钟被显式滞后 2 ms）。
#[test]
fn a_timer_registered_on_a_live_clock_is_anchored_to_the_registration_instant() {
    let mut app = TestApp::new();
    let window = app.window_id();
    // 平台泵轮首：引擎时钟推进到真实时间并标记为活时钟。
    app.app_mut().sync_clock_live();
    // 模拟「泵起点 → 应用登记」之间的前置工作：引擎时钟落在登记瞬间之后。
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

/// **E10c-R01 回归**：`Application::run` **之前**（首次泵启动前）登记的定时器同样只能晚不能早。
///
/// 审查探针（Windows 真机）：App 构造 → 建窗 → 80 ms 初始化 → 登记 1500 ms 定时器，修复前
/// 触发于 `+1430.42 ms`（**提前 69.58 ms**）—— deadline 用了 `App::new` 的时钟，构造 → 登记
/// 之间的初始化耗时被算进了 `delay`。修复后 `App::new` 就是活时钟 ⇒ 基准 = 登记瞬间。
///
/// 变异守卫：`App::new` 的 `clock_live` 改回 false ⇒ 第一条断言变红（`App::new` 与登记之间
/// 的初始化耗时被算进 delay，deadline 落在 `登记瞬间 + delay` 之前）。
#[test]
fn a_timer_registered_before_the_first_pump_is_not_early() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions::default());
    // 模拟 `Application::run` 之前的初始化（建窗 / 建树 / 资源准备）耗时。
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

    // 泵启动之后的登记保持同一语义（活时钟由泵每轮刷新）。
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

/// **E10c 的 I3 断言**：等待一次性定时器期间，泵轮不提交帧、不请求帧、不派发未到期的定时器
/// （Windows 同代码 1 轮；macOS 修复前被 display link 的 vsync tick 空转 54 轮）。
///
/// E10c2-R05：等待期只推进**引擎时钟**（`TestApp::advance`，可控、无真实 sleep），断言与真实
/// 时间无关 —— 高负载 CI 上不会因为一次调度停顿就让定时器在断言前到期（旧版真实
/// `sleep(10ms)×3` 的 flake 风险）。
///
/// 变异守卫（E10c M2）：让 macOS 泵的空闲轮判定恒为真（`platform::macos_pump::app_round`
/// 恒 true）⇒ 真机 `timer_smoke` 的「等待期应用轮数」失败；本用例守护引擎侧的同一条不变量
/// （等待期间 `App::frame` 恒 `Idle`、`FrameIndex` 不变、无帧请求）。
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

    // 等待期：只推进引擎时钟（确定性），未到期 ⇒ 恒 Idle。
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

    // 到点后由下一轮的钩子步骤派发一次；表清空、不重复。
    app.advance(Duration::from_millis(90));
    app.app_mut().run_frame_hook();
    assert!(fired.get(), "到点必须派发");
    assert_eq!(app.app().timer_count_for_test(), 0);
    app.app_mut().run_frame_hook();
    assert_eq!(app.app().timer_count_for_test(), 0, "只派发一次");
}

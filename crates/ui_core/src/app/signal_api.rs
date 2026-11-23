use std::any::Any;

use super::App;
#[cfg(feature = "test-support")]
use crate::node::dependency::DependencyGraph;
use crate::node::{
    dependency::ReadCtx,
    signal::{Derived, Signal, SignalRef, SignalScope, Transient},
};

#[cfg(feature = "test-support")]
pub(crate) fn dependency_graph(app: &App) -> &DependencyGraph {
    &app.graph
}

pub(crate) fn signal<T: Clone + 'static>(app: &mut App, initial: T) -> Signal<T> {
    let id = app.store.push_owned(initial);
    Signal::new(id)
}

pub(crate) fn signal_scoped<T: Clone + 'static>(
    app: &mut App,
    scope: SignalScope,
    initial: T,
) -> Signal<T> {
    let id = app.store.push_owned_scoped(scope, initial);
    Signal::new(id)
}

pub(crate) fn transient<T: Clone + 'static>(app: &mut App, initial: T) -> Transient<T> {
    let id = app.store.push_transient(initial);
    Transient::new(id)
}

pub(crate) fn transient_scoped<T: Clone + 'static>(
    app: &mut App,
    scope: SignalScope,
    initial: T,
) -> Transient<T> {
    let id = app.store.push_transient_scoped(scope, initial);
    Transient::new(id)
}

pub(crate) fn derived<T: Clone + 'static>(
    app: &mut App,
    compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
) -> Derived<T> {
    let boxed: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>> =
        Box::new(move |cx| Box::new(compute(cx)));
    let (id, _handle) = app.store.push_derived(boxed);
    Derived::new(id)
}

pub(crate) fn signal_scope(app: &mut App) -> SignalScope {
    app.store.new_scope()
}

pub(crate) fn release_scope(app: &mut App, scope: SignalScope) -> usize {
    let Some(signals) = app.store.release_scope(scope) else {
        return 0;
    };
    let released = signals.len();
    if !signals.is_empty() {
        super::animation::release_scope_animations(app, &signals);
    }
    for signal in signals {
        app.graph.remove_signal(signal);
    }
    released
}

pub(crate) fn write<T: Clone + PartialEq + 'static>(
    app: &mut App,
    signal: &Signal<T>,
    value: T,
) -> bool {
    let id = signal.id();
    match app.store.write_owned(id, value) {
        None => false,
        Some(false) => false,
        Some(true) => {
            app.invalidate(id);
            true
        }
    }
}

pub(crate) fn write_with<T: Clone + PartialEq + 'static>(
    app: &mut App,
    signal: &Signal<T>,
    body: impl FnOnce(&T) -> T,
) -> bool {
    let id = signal.id();
    if !app.store.is_live(id) {
        return false;
    }
    let value = {
        let guard = app.store.read_owned::<T>(id);
        body(&guard)
    };
    write(app, signal, value)
}

pub(crate) fn update<T: 'static>(
    app: &mut App,
    signal: &Signal<T>,
    update: impl FnOnce(&mut T),
) -> bool {
    let id = signal.id();
    match app.store.update_owned(id, update) {
        None => false,
        Some(()) => {
            app.invalidate(id);
            true
        }
    }
}

pub(crate) fn update_with<T: 'static, R>(
    app: &mut App,
    signal: &Signal<T>,
    body: impl FnOnce(&mut T) -> R,
) -> Option<R> {
    let id = signal.id();
    let result = app.store.update_owned(id, body)?;
    app.invalidate(id);
    Some(result)
}

pub(crate) fn write_transient<T: 'static>(app: &mut App, transient: &Transient<T>, value: T) {
    app.store.set_transient(transient.id(), value);
}

#[must_use]
pub(crate) fn read<T: 'static>(app: &App, signal: &Signal<T>) -> SignalRef<T> {
    app.store.read_owned(signal.id())
}

pub(crate) fn read_with<T: 'static, R>(
    app: &App,
    signal: &Signal<T>,
    body: impl FnOnce(&T) -> R,
) -> R {
    let guard = app.store.read_owned::<T>(signal.id());
    body(&guard)
}

#[must_use]
pub(crate) fn read_transient<T: 'static>(app: &App, transient: &Transient<T>) -> SignalRef<T> {
    app.store.read_transient(transient.id())
}

#[cfg(test)]
pub(crate) fn eval_stack_is_empty_for_test(app: &App) -> bool {
    app.eval_stack.is_empty()
}

#[cfg(test)]
pub(crate) fn dependency_edge_count(app: &App) -> usize {
    app.graph.signal_count()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, panic::AssertUnwindSafe};

    use crate::{
        app::App,
        node::{
            builder::NodeBuilder,
            dependency::ReadCtx,
            dirty::DirtyFlags,
            id::NodeId,
            signal::{Signal, SignalScope},
        },
        paint::style::StylePatch,
        window::{WindowId, WindowOptions},
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Mode {
        Idle,
        Busy,
    }

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(rt.handle().clone());
        (rt, app)
    }

    fn build_node<F>(app: &mut App, window: WindowId, bind: F) -> NodeId
    where
        F: Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
    {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::boxed()
            .bind_style(bind)
            .build(w.tree_mut(), root)
            .expect("挂载成功")
    }

    fn dirty(app: &App, window: WindowId, node: NodeId) -> DirtyFlags {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
    }

    fn clear_dirty(app: &mut App, window: WindowId) {
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .clear_all_dirty_for_test();
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&'static str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_default()
    }

    #[test]
    fn read_with_returns_the_closure_value_and_drops_the_guard_immediately() {
        let (_rt, mut app) = new_app();
        let signal: Signal<String> = app.signal(String::from("BTC"));

        let shout = app.read_with(&signal, |value| format!("{value}!"));
        assert_eq!(shout, "BTC!", "组合子必须把闭包返回值原样透出");
        assert_eq!(&*app.read(&signal), "BTC");

        assert!(
            app.write(&signal, String::from("ETH")),
            "read_with 返回后守卫必须已析构 ⇒ 写同一信号不得 panic"
        );
        assert!(app.update(&signal, |value| value.push('!')));
        assert_eq!(&*app.read(&signal), "ETH!");
    }

    #[test]
    fn update_with_returns_the_closure_value_and_invalidates() {
        let (_rt, mut app) = new_app();
        let signal: Signal<Vec<u32>> = app.signal(vec![1, 2, 3]);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some((*cx.read(&signal)).len() as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(
            app.dependency_edge_count_for_test(),
            1,
            "节点对该信号有一条依赖边"
        );
        clear_dirty(&mut app, window);
        let _ = app.take_wake_request();

        let len = app.update_with(&signal, |values| {
            values.push(4);
            values.len()
        });
        assert_eq!(len, Some(4), "Some(r) = 闭包返回值，且修改已就地生效");
        assert_eq!(*app.read(&signal), vec![1, 2, 3, 4]);
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "update_with 必须沿依赖图失效"
        );
        assert!(app.take_wake_request(), "有节点被标脏 ⇒ 必须请求唤醒");
        assert_eq!(
            app.dependency_edge_count_for_test(),
            1,
            "失效不得删除依赖边"
        );

        clear_dirty(&mut app, window);
        let _ = app.take_wake_request();
        let same = app.update_with(&signal, |values| {
            *values = vec![1, 2, 3, 4];
            values.len()
        });
        assert_eq!(same, Some(4), "update_with 没有等值短路");
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "等值也必须失效（无等值短路是 update 的公开语义）"
        );
    }

    #[test]
    fn update_with_is_a_noop_on_a_released_scope() {
        let (_rt, mut app) = new_app();
        let scope: SignalScope = app.signal_scope();
        let signal: Signal<u32> = app.signal_scoped(scope, 7);
        assert_eq!(app.release_scope(scope), 1, "作用域释放成功");

        let called = Cell::new(false);
        let result = app.update_with(&signal, |value| {
            called.set(true);
            *value += 1;
        });
        assert_eq!(
            result, None,
            "作用域已释放 ⇒ None（与 `update` 返回 false 同义）"
        );
        assert!(!called.get(), "no-op 不得调用闭包（一次借用都不做）");
        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = *app.read(&signal);
        }))
        .expect_err("失效句柄的读必须 panic");
        assert!(panic_message(payload).contains("stale"));
    }

    #[test]
    fn thousand_window_lifecycles_keep_the_slot_peak_flat_and_leave_no_edges() {
        const CYCLES: usize = 1000;
        const SIGNALS_PER_WINDOW: usize = 2;

        let (_rt, mut app) = new_app();
        let mut peak: Option<usize> = None;
        for cycle in 0..CYCLES {
            let window = app.create_window(WindowOptions::default());
            let first: Signal<u32> = app.window_signal(window, 1);
            let second: Signal<u32> = app.window_signal(window, 2);
            assert!(app.write(&first, 3));
            assert!(app.write(&second, 4));
            let _node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
                opacity: Some(*cx.read(&first) as f32 + *cx.read(&second) as f32),
                ..StylePatch::default()
            });
            app.refresh_styles(window).expect("窗口存在");
            assert_eq!(
                app.dependency_edge_count_for_test(),
                SIGNALS_PER_WINDOW,
                "第 {cycle} 轮节点必须对两个窗口信号各有一条依赖边"
            );
            assert!(app.close_window(window), "关窗成功");
            assert_eq!(
                app.dependency_edge_count_for_test(),
                0,
                "第 {cycle} 轮关窗后不得有残留依赖边"
            );
            let slots = app.store.slot_count_for_test();
            match peak {
                Some(expected) => assert_eq!(
                    slots, expected,
                    "第 {cycle} 轮后信号槽位峰值不得增长（回收失效会线性增长）"
                ),
                None => peak = Some(slots),
            }
        }
        assert_eq!(
            app.store.free_slot_count_for_test(),
            SIGNALS_PER_WINDOW,
            "两轮槽位必须全部回到空闲栈"
        );
        assert_eq!(
            app.store.live_slot_count_for_test(),
            0,
            "没有存活信号占着槽位"
        );
    }

    #[test]
    fn window_signal_handle_is_stale_after_close_and_never_reads_the_reused_slot() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let stale: Signal<u32> = app.window_signal(first, 11);
        assert!(app.close_window(first));

        let second = app.create_window(WindowOptions::default());
        let fresh: Signal<u32> = app.window_signal(second, 22);
        assert_eq!(
            fresh.id().index(),
            stale.id().index(),
            "关窗后槽位必须被复用"
        );
        assert_eq!(
            fresh.id().generation(),
            stale.id().generation() + 1,
            "复用必须推进代际"
        );

        assert!(!app.write(&stale, 99));
        assert!(!app.update(&stale, |value| *value = 99));
        assert_eq!(*app.read(&fresh), 22, "旧句柄绝不能写到复用后的新值");

        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = *app.read(&stale);
        }))
        .expect_err("旧句柄读必须 panic");
        assert!(panic_message(payload).contains("stale"));

        assert!(app.write(&fresh, 23));
        assert_eq!(*app.read(&fresh), 23);
    }

    #[test]
    fn write_with_on_a_stale_handle_returns_false_without_calling_the_closure() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let stale: Signal<u32> = app.window_signal(window, 1);
        assert!(app.close_window(window));

        let called = Cell::new(false);
        assert!(
            !app.write_with(&stale, |old| {
                called.set(true);
                old + 1
            }),
            "失效句柄的 write_with 必须是 no-op"
        );
        assert!(!called.get(), "no-op 不得调用闭包");
    }

    #[test]
    fn stale_handle_write_cannot_invalidate_the_reused_slots_readers() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let stale: Signal<u32> = app.window_signal(first, 1);
        assert!(app.close_window(first));

        let second = app.create_window(WindowOptions::default());
        let fresh: Signal<u32> = app.window_signal(second, 10);
        assert_eq!(fresh.id().index(), stale.id().index(), "槽位已复用");
        let node = build_node(&mut app, second, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&fresh) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(second).expect("窗口存在");
        assert_eq!(
            app.dependency_edge_count_for_test(),
            1,
            "新信号有一条节点依赖边"
        );
        clear_dirty(&mut app, second);
        let _ = app.take_wake_request();

        assert!(!app.write(&stale, 99), "旧句柄的写是 no-op");
        assert!(
            !dirty(&app, second, node).contains(DirtyFlags::PAINT),
            "旧句柄不得沿复用槽位的新依赖边失效任何读者"
        );
        assert!(!app.take_wake_request(), "不得请求帧（无僵尸唤醒）");
        assert_eq!(
            app.dependency_edge_count_for_test(),
            1,
            "新信号的依赖边不受旧句柄影响"
        );
        assert_eq!(*app.read(&fresh), 10, "新占用者的值未被改动");

        assert!(app.write(&fresh, 11), "新句柄照常失效");
        assert!(dirty(&app, second, node).contains(DirtyFlags::PAINT));
    }

    #[test]
    fn write_with_produces_the_new_value_from_the_old_one() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(10);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        clear_dirty(&mut app, window);
        let _ = app.take_wake_request();

        let seen = Cell::new(0u32);
        assert!(
            app.write_with(&signal, |old| {
                seen.set(*old);
                old + 5
            }),
            "值变化 ⇒ true"
        );
        assert_eq!(seen.get(), 10, "闭包必须拿到旧值");
        assert_eq!(*app.read(&signal), 15);
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "值变化必须沿依赖图失效"
        );

        clear_dirty(&mut app, window);
        let _ = app.take_wake_request();
        assert!(
            !app.write_with(&signal, |old| *old),
            "等值 ⇒ false（与 `write` 的等值短路同义）"
        );
        assert!(
            !dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "等值短路不得失效"
        );
        assert!(!app.take_wake_request(), "等值短路不得请求唤醒");
        assert_eq!(*app.read(&signal), 15);
    }

    #[test]
    fn the_match_scrutinee_guard_panics_on_write_but_update_with_does_not() {
        let (_rt, mut app) = new_app();
        let signal: Signal<Mode> = app.signal(Mode::Idle);

        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| match *app.read(&signal) {
            Mode::Idle => app.write(&signal, Mode::Busy),
            Mode::Busy => false,
        }))
        .expect_err("match 头的读守卫存活期间写同一信号必须 panic");
        let message = panic_message(payload);
        assert!(
            message.contains("SignalRef read guard"),
            "panic 必须点名读守卫（可诊断）：{message}"
        );
        assert_eq!(*app.read(&signal), Mode::Idle, "冲突的写不得生效");

        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                if *app.read(&signal) == Mode::Idle {
                    app.update(&signal, |mode| *mode = Mode::Busy);
                }
            }))
            .is_ok(),
            "if 条件的临时守卫在进入 body 之前析构"
        );
        assert_eq!(*app.read(&signal), Mode::Busy);

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            app.update_with(&signal, |mode| match *mode {
                Mode::Idle => *mode = Mode::Busy,
                Mode::Busy => *mode = Mode::Idle,
            })
        }));
        assert!(result.is_ok(), "update_with 消灭 match 陷阱");
        assert_eq!(
            result.expect("不 panic"),
            Some(()),
            "update_with 返回闭包的值"
        );
        assert_eq!(
            *app.read(&signal),
            Mode::Idle,
            "闭包里的 match 已完成状态迁移"
        );
    }

    #[test]
    fn the_trap_lives_in_the_outer_guard_not_in_the_write_api() {
        let (_rt, mut app) = new_app();
        let signal: Signal<Mode> = app.signal(Mode::Idle);

        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| match *app.read(&signal) {
            Mode::Idle => app.update_with(&signal, |mode| *mode = Mode::Busy),
            Mode::Busy => None,
        }))
        .expect_err("外层 match 的读守卫仍存活 ⇒ update_with 同样 panic");
        assert!(
            panic_message(payload).contains("SignalRef read guard"),
            "panic 必须点名读守卫"
        );
        assert_eq!(*app.read(&signal), Mode::Idle, "冲突的写不得生效");

        let mode = *app.read(&signal);
        match mode {
            Mode::Idle => assert!(app.write(&signal, Mode::Busy)),
            Mode::Busy => panic!("初始值必须是 Idle"),
        }
        assert_eq!(*app.read(&signal), Mode::Busy);
    }
}

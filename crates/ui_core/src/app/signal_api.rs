//! 信号 API：注册 / 读写 / 派生 / 依赖图访问（写路径的失效入口在 `style_pass`）。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use std::any::Any;

use super::App;
#[cfg(feature = "test-support")]
use crate::node::dependency::DependencyGraph;
use crate::node::{
    dependency::ReadCtx,
    signal::{Derived, Signal, SignalRef, SignalScope, Transient},
};

/// 依赖图（只读；`test-support` 的 `diagnostics::dependency_dump` 用）。
/// 消费者：`App::dependency_graph`。
#[cfg(feature = "test-support")]
pub(crate) fn dependency_graph(app: &App) -> &DependencyGraph {
    &app.graph
}

// ── 状态注册 ──────────────────────────────────────────────────────────────

/// 建一个普通信号（归属**根作用域**：永不释放，因此它的槽位永远不会被回收）。
/// 消费者：`App::signal`。
pub(crate) fn signal<T: Clone + 'static>(app: &mut App, initial: T) -> Signal<T> {
    let id = app.store.push_owned(initial);
    Signal::new(id)
}

/// 建一个**作用域内**的普通信号（窗口 / 面板 / 对话框的生命周期）。
///
/// 槽位优先复用空闲栈（作用域释放回收的下标 + 已推进的代际），因此**句柄不保证重建**：
/// 释放后旧句柄的读 panic、写 no-op（见 [`SignalScope`]）。作用域已释放时 panic（引擎误用）。
/// 消费者：`App::signal_scoped` / `App::window_signal`。
pub(crate) fn signal_scoped<T: Clone + 'static>(
    app: &mut App,
    scope: SignalScope,
    initial: T,
) -> Signal<T> {
    let id = app.store.push_owned_scoped(scope, initial);
    Signal::new(id)
}

/// 建一个瞬态值（读不登记、写不失效）。
/// 消费者：`App::transient`。
pub(crate) fn transient<T: Clone + 'static>(app: &mut App, initial: T) -> Transient<T> {
    let id = app.store.push_transient(initial);
    Transient::new(id)
}

/// 注册一个派生值（**不**立即求值；环在运行期检测）。
/// 消费者：`App::derived`。
///
/// 派生归属**根作用域**：闭包捕获作用域信号会在释放后留下"永久陈旧 + 下次重算 panic"的组合
/// （见 `Derived` / `SignalScope` 的文档，R5c）。这里不（也无法）检查不透明闭包。
pub(crate) fn derived<T: Clone + 'static>(
    app: &mut App,
    compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
) -> Derived<T> {
    let boxed: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>> =
        Box::new(move |cx| Box::new(compute(cx)));
    let (id, _handle) = app.store.push_derived(boxed);
    Derived::new(id)
}

/// 新建一个信号作用域（窗口 / 面板 / 对话框的自有生命周期）。
/// 消费者：`App::signal_scope`。
pub(crate) fn signal_scope(app: &mut App) -> SignalScope {
    app.store.new_scope()
}

/// 释放信号作用域：其值**立即回收**（槽位归还空闲栈、代际 +1），依赖图里以它们为键的边
/// **立即**删除，旧句柄从此读 panic / 写 no-op（见 [`SignalScope`]）。
///
/// 返回被释放的信号数。重复释放返回 0（幂等）；根作用域不会被释放（返回 0）。
/// 消费者：`App::release_scope` / `App::close_window`。
///
/// 只清值槽位与依赖边：**根作用域里捕获了这些句柄的派生不会被失效**（旧边已删），下次因其它
/// 依赖重算会读死句柄 panic —— 跨作用域捕获是应用层硬禁令（R5c，见 `Derived` 文档）。
pub(crate) fn release_scope(app: &mut App, scope: SignalScope) -> usize {
    let Some(signals) = app.store.release_scope(scope) else {
        return 0;
    };
    let released = signals.len();
    for signal in signals {
        // 立即删边（不是"等下次写时惰性清理"）：释放的价值就是之后的写不再遍历任何 owner。
        // 键是完整 `SignalId`（下标 + 代际），复用后的新信号代际不同 ⇒ 不会继承这些边。
        app.graph.remove_signal(signal);
    }
    released
}

// ── 写路径 ────────────────────────────────────────────────────────────────

/// 写一个信号。返回 `true` = 值变了（并已沿依赖图失效）；`false` = 值相等（短路）
/// **或**该句柄已失效（作用域已释放 / 槽位已复用：no-op，不标脏、不唤醒）。
///
/// 返回 `false` 的第二种情况**不可**与等值短路区分 —— 这是有意的：作用域释放后的迟到写入
/// （异步任务 / 定时器回投）必须无害（R03-6 的僵尸唤醒防护），而句柄失效本身是应用 bug。
/// 同一句柄的**读**会确定性 panic（见 [`read`]）。
///
/// 返回值不表示「有节点被标脏」（只写派生时没有节点变脏）。
///
/// **应用层契约**：`T` 必须是不可变值语义。若 `T` 内部可变（`Rc<RefCell<_>>`/`Cell`/`Mutex`）而
/// `PartialEq` 只比较外层，则内部变化会被短路吞掉（不重绘）。见 S-35 / R9。
///
/// # Panics
/// 该信号存在活跃 [`SignalRef`] 读守卫（借用冲突是显式失败，不静默拷贝，见 `node::signal` 模块文档）。
/// 消费者：`App::write`。
pub(crate) fn write<T: Clone + PartialEq + 'static>(
    app: &mut App,
    signal: &Signal<T>,
    value: T,
) -> bool {
    let id = signal.id();
    match app.store.write_owned(id, value) {
        None => false,        // 句柄已失效：no-op（不失效、不唤醒、不写复用后的新值）
        Some(false) => false, // 等值短路
        Some(true) => {
            app.invalidate(id);
            true
        }
    }
}

/// 读旧值 → 由闭包产出新值 → 按 [`write`] 的语义写（等值短路 / 句柄已失效 ⇒ `false`）。
///
/// **守卫必须在 `write_owned` 之前析构**，否则这次写会撞上自己的读守卫（panic）：块作用域
/// 保证闭包返回的那一刻守卫就 drop，随后才写。
///
/// 句柄已失效时**不调用闭包**、直接返回 `false`：它是「先读旧值再写」组合子，读那一步不能把
/// 迟到写入变成 UI 线程 panic（与 [`write`] 的 no-op 同口径）。
///
/// # Panics
/// 同 [`write`]（活跃读守卫 / 类型不符）。
/// 消费者：`App::write_with`。
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

/// 原地更新一个信号（**无等值短路**：调用方声明这次修改必须生效）。
///
/// 适合大对象（列表 / 图）的局部修改：不 clone 旧值、不做 `PartialEq` 比较，改完直接失效。
/// 返回 `false` 仅表示该句柄已失效（no-op，闭包不会被调用）。
///
/// # Panics
/// 同 [`write`]（活跃读守卫 / 类型不符）。
/// 消费者：`App::update`。
pub(crate) fn update<T: 'static>(
    app: &mut App,
    signal: &Signal<T>,
    update: impl FnOnce(&mut T),
) -> bool {
    let id = signal.id();
    match app.store.update_owned(id, update) {
        None => false, // 句柄已失效：no-op
        Some(()) => {
            app.invalidate(id);
            true
        }
    }
}

/// 在闭包内就地更新一个信号并沿依赖图失效，返回闭包的返回值。
///
/// `None` = 句柄已失效（no-op，闭包**不会**被调用）；`Some(r)` = 已就地改完并已失效。
///
/// # Panics
/// 同 [`update`]（活跃读守卫 / 类型不符）。
/// 消费者：`App::update_with`。
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

/// 写一个瞬态值：**不**失效、**不**请求唤醒（滚动偏移/悬浮态，§3.7）。
///
/// # Panics
/// 存在活跃读守卫 / 槽位不是瞬态值。
/// 消费者：`App::write_transient`。
pub(crate) fn write_transient<T: 'static>(app: &mut App, transient: &Transient<T>, value: T) {
    app.store.set_transient(transient.id(), value);
}

/// 非追踪读一个信号，返回**读守卫**（零拷贝）。
///
/// # Panics
/// - 句柄已失效（作用域已释放 / 槽位已回收复用）：**确定性失败**，绝不返回复用后的新值
///   （见 `node::signal` 模块文档〈作用域回收与句柄失效〉）；
/// - `signal` 的槽位不是普通信号（引擎不变量被破坏时才会发生）。
///
/// 消费者：`App::read`。
#[must_use]
pub(crate) fn read<T: 'static>(app: &App, signal: &Signal<T>) -> SignalRef<T> {
    app.store.read_owned(signal.id())
}

/// 在闭包内读一个信号：守卫在调用返回时析构（**不可逃逸**）。
///
/// # Panics
/// 同 [`read`]（句柄已失效 / 槽位不是普通信号）。
/// 消费者：`App::read_with`。
pub(crate) fn read_with<T: 'static, R>(
    app: &App,
    signal: &Signal<T>,
    body: impl FnOnce(&T) -> R,
) -> R {
    let guard = app.store.read_owned::<T>(signal.id());
    body(&guard)
}

/// 非追踪读一个瞬态值，返回读守卫（动画进度 / 滚动偏移的公开读取面）。
///
/// # Panics
/// 句柄已失效，或槽位不是瞬态值（引擎不变量被破坏时才会发生）。
/// 消费者：`App::read_transient`。
#[must_use]
pub(crate) fn read_transient<T: 'static>(app: &App, transient: &Transient<T>) -> SignalRef<T> {
    app.store.read_transient(transient.id())
}

/// 仅测试：求值栈是否为空（验证 `EvalGuard` 在 panic 后仍然出栈）。
/// 消费者：`App::eval_stack_is_empty_for_test`。
#[cfg(test)]
pub(crate) fn eval_stack_is_empty_for_test(app: &App) -> bool {
    app.eval_stack.is_empty()
}

/// 仅测试：依赖图当前的表项数（作用域释放「立即删边」的可测证据）。
/// 消费者：`App::dependency_edge_count_for_test`。
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

    /// 枚举状态机的状态（复审 §2.2 的 `match` 陷阱就出在这种类型上）。
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

    // ── read_with：守卫不逃逸 ──────────────────────────────────────────────

    #[test]
    fn read_with_returns_the_closure_value_and_drops_the_guard_immediately() {
        let (_rt, mut app) = new_app();
        let signal: Signal<String> = app.signal(String::from("BTC"));

        // 返回值与守卫的寿命无关：非 Copy 的返回值（String）也能从闭包里带出来。
        let shout = app.read_with(&signal, |value| format!("{value}!"));
        assert_eq!(shout, "BTC!", "组合子必须把闭包返回值原样透出");
        assert_eq!(&*app.read(&signal), "BTC");

        // 守卫没有泄漏到调用点：紧接着写同一个信号不 panic
        // （手写 `let g = app.read(&signal); ... app.write(..)` 会 panic）。
        assert!(
            app.write(&signal, String::from("ETH")),
            "read_with 返回后守卫必须已析构 ⇒ 写同一信号不得 panic"
        );
        assert!(app.update(&signal, |value| value.push('!')));
        assert_eq!(&*app.read(&signal), "ETH!");
    }

    // ── update_with：返回值 + 失效 + 无等值短路 ────────────────────────────

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

        // Some(r)：闭包的返回值被透出；值就地改完；读者被标脏并请求唤醒。
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

        // 无等值短路：写回**相同的值**也返回 Some 并再次失效（与 `update` 同口径）。
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

    // ── update_with：已释放作用域是 no-op ─────────────────────────────────

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
        // 值已随作用域释放被回收：旧句柄的读是确定性失败（不再是"返回冻结值"）。
        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = *app.read(&signal);
        }))
        .expect_err("失效句柄的读必须 panic");
        assert!(panic_message(payload).contains("stale"));
    }

    // ── S6：窗口作用域的槽位回收（App 级）─────────────────────────────────

    /// 1000 轮「建窗 → window_signal → 依赖边 → 关窗」：槽位峰值与依赖边都不增长。
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

    /// 关窗后槽位被复用：旧句柄的读确定性失败、写 no-op，绝不命中新窗口的值。
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

        // 写 / 更新：no-op，绝不改到新占用者。
        assert!(!app.write(&stale, 99));
        assert!(!app.update(&stale, |value| *value = 99));
        assert_eq!(*app.read(&fresh), 22, "旧句柄绝不能写到复用后的新值");

        // 读：确定性失败。
        let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = *app.read(&stale);
        }))
        .expect_err("旧句柄读必须 panic");
        assert!(panic_message(payload).contains("stale"));

        // 新句柄照常工作。
        assert!(app.write(&fresh, 23));
        assert_eq!(*app.read(&fresh), 23);
    }

    /// `write_with` 是「先读后写」组合子：失效句柄不得在读一步 panic，而是 no-op（闭包不执行）。
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

    /// 旧句柄的写不得沿**复用槽位的新信号**的依赖边失效任何读者（槽位级边隔离）。
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

    // ── write_with：新值由旧值派生 + 等值短路 ─────────────────────────────

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

        // 不同 ⇒ true（并失效）。闭包拿到的是**旧值**；守卫必须在写之前析构，否则这里就 panic。
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

        // 等值 ⇒ false（短路：不失效、不唤醒），且不 panic。
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

    // ── match 陷阱：复审 §2.2 的两个写法 ──────────────────────────────────

    #[test]
    fn the_match_scrutinee_guard_panics_on_write_but_update_with_does_not() {
        let (_rt, mut app) = new_app();
        let signal: Signal<Mode> = app.signal(Mode::Idle);

        // ① 复审实测的写法：`match` 的 scrutinee 临时量活到**整个 match** 结束，
        //    分支里的写撞上仍然存活的读守卫 ⇒ 必 panic（编译期无法提醒）。
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

        // 对照：`if` 条件是 terminating scope ⇒ 同样的「读 + 写」不 panic。
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

        // ② 正确写法：把 match 搬进 `update_with` 的闭包 —— 闭包拿 `&mut Mode`，没有读守卫，
        //    因此不 panic 且值真的更新。枚举状态机的最自然写法也能这么写。
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

    /// 陷阱的根因是**外层读守卫**，不是写入口的 API：即使分支里改用 `update_with`，
    /// 只要读守卫仍在 `match` 头里活着，就地写同样会 panic（组合子不能隔着外层守卫工作）。
    /// 正确做法只有两条：把读搬进闭包，或在 `match` 之前把值取出来（`let mode = *cx.read(&s);`）。
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

        // 先把值 Copy 出来（守卫在那条语句结束时析构）⇒ 之后的写安全。
        let mode = *app.read(&signal);
        match mode {
            Mode::Idle => assert!(app.write(&signal, Mode::Busy)),
            Mode::Busy => panic!("初始值必须是 Idle"),
        }
        assert_eq!(*app.read(&signal), Mode::Busy);
    }
}

//! 信号 API：注册 / 读写 / 派生 / 依赖图访问（写路径的失效入口在 `style_pass`）。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use std::any::Any;

use super::App;
#[cfg(feature = "test-support")]
use crate::node::dependency::DependencyGraph;
use crate::node::{
    dependency::ReadCtx,
    signal::{Derived, Signal, Transient},
};

/// 依赖图（只读；`test-support` 的 `diagnostics::dependency_dump` 用）。
/// 消费者：`App::dependency_graph`。
#[cfg(feature = "test-support")]
pub(crate) fn dependency_graph(app: &App) -> &DependencyGraph {
    &app.graph
}

// ── 状态注册 ──────────────────────────────────────────────────────────────

/// 建一个普通信号（id 稠密递增，条目永不删除）。
/// 消费者：`App::signal`。
pub(crate) fn signal<T: Clone + 'static>(app: &mut App, initial: T) -> Signal<T> {
    let id = app.store.push_owned(initial);
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
pub(crate) fn derived<T: Clone + 'static>(
    app: &mut App,
    compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
) -> Derived<T> {
    let boxed: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>> =
        Box::new(move |cx| Box::new(compute(cx)));
    let (id, _handle) = app.store.push_derived(boxed);
    Derived::new(id)
}

// ── 写路径 ────────────────────────────────────────────────────────────────

/// 写一个信号。返回 `true` = 值变了（并已沿依赖图失效）；`false` = 值相等，**短路**。
///
/// 返回值不表示「有节点被标脏」（只写派生时没有节点变脏）。
///
/// **应用层契约**：`T` 必须是不可变值语义。若 `T` 内部可变（`Rc<RefCell<_>>`/`Cell`/`Mutex`）而
/// `PartialEq` 只比较外层，则内部变化会被短路吞掉（不重绘）。见 S-35 / R9。
/// 消费者：`App::write`。
pub(crate) fn write<T: Clone + PartialEq + 'static>(
    app: &mut App,
    signal: &Signal<T>,
    value: T,
) -> bool {
    let id = signal.id();
    if app.store.owned_ref::<T>(id) == Some(&value) {
        return false;
    }
    app.store.set_owned(id, value);
    app.invalidate(id);
    true
}

/// 写一个瞬态值：**不**失效、**不**请求唤醒（滚动偏移/悬浮态，§3.7）。
/// 消费者：`App::write_transient`。
pub(crate) fn write_transient<T: 'static>(app: &mut App, transient: &Transient<T>, value: T) {
    app.store.set_transient(transient.id(), value);
}

/// 非追踪读一个信号（handler 不是渲染 pass，不登记依赖）。
///
/// # Panics
/// `signal` 的槽位不是普通信号（引擎不变量被破坏时才会发生）。
/// 消费者：`App::read`。
#[must_use]
pub(crate) fn read<T: Clone + 'static>(app: &App, signal: &Signal<T>) -> T {
    app.store
        .owned_ref::<T>(signal.id())
        .cloned()
        .expect("signal slot is not a plain signal")
}

/// 非追踪读一个瞬态值（动画进度 / 滚动偏移的公开读取面）。
///
/// # Panics
/// 槽位不是瞬态值（引擎不变量被破坏时才会发生）。
/// 消费者：`App::read_transient`。
#[must_use]
pub(crate) fn read_transient<T: Clone + 'static>(app: &App, transient: &Transient<T>) -> T {
    app.store
        .transient_ref::<T>(transient.id())
        .cloned()
        .expect("signal slot is not a transient value")
}

/// 仅测试：求值栈是否为空（验证 `EvalGuard` 在 panic 后仍然出栈）。
/// 消费者：`App::eval_stack_is_empty_for_test`。
#[cfg(test)]
pub(crate) fn eval_stack_is_empty_for_test(app: &App) -> bool {
    app.eval_stack.is_empty()
}

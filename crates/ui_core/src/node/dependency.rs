//! 依赖追踪：`Owner`/`DependencyGraph`/`ReadTracker`/`ReadCtx`/`EvalStack`/`EvalGuard`。
//!
//! 要点：
//! - 依赖图只用一个键空间：`SignalId → OwnerSet`（派生值也有自己的句柄槽 `SignalId`）；
//! - 读取集合是**本帧替换**（不是累积），差分清理见 `replace_deps`；
//! - 环检测在运行时求值栈上，入/出栈由 `EvalGuard` 的 `Drop` 保证 panic 安全。

use std::{any::Any, rc::Rc};

use smallvec::SmallVec;

use crate::{
    error::EngineError,
    node::{
        id::NodeId,
        signal::{
            Derived, DerivedId, DerivedStore, Signal, SignalId, SignalRef, SignalSlot, Transient,
        },
    },
    window::WindowId,
};

/// 依赖边的**下游**：节点（带窗口，`NodeId` 是窗口内 id，必须消歧）或另一个派生值。
///
/// `Copy + Eq + Hash`：可作集合元素、可 `retain`/`contains`/复制出借用。**不**实现 `Ord`
/// （无排序消费者；`NodeId` 也不实现 `Ord`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Owner {
    Node { window: WindowId, node: NodeId },
    Derived(DerivedId),
}

/// 一个信号的读者集合。`SmallVec`（内联 2 个）而非 `HashSet`/位图：
/// 稳态读者 1–3 个、零分配、迭代序确定，且位图会在「index 复用 + 代际」下误命中。
pub(crate) type OwnerSet = SmallVec<[Owner; 2]>;

/// `SignalId → 读者集合` 的反查表。
///
/// 不变量（§8.3 D1–D5）：每条边唯一；(信号, owner) 至多一份；空集合立即删表项；
/// 已卸载的 owner 允许残留，由 `App::write` 的 `retain` 惰性剔除。
#[derive(Default)]
pub(crate) struct DependencyGraph {
    dependents: ahash::HashMap<SignalId, OwnerSet>,
}

impl DependencyGraph {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 可变反查（`App::write` 的第一跳 + 死 owner 剔除）。
    pub(crate) fn dependents_mut(&mut self, id: SignalId) -> Option<&mut OwnerSet> {
        self.dependents.get_mut(&id)
    }

    /// 只读反查表（消费者：`test-support` 的 `diagnostics::dependency_dump`；P13）。
    #[cfg(feature = "test-support")]
    pub(crate) fn dependents(&self) -> &ahash::HashMap<SignalId, OwnerSet> {
        &self.dependents
    }

    /// **每帧替换**依赖：`old` 有新无 → 退订；`new` 有旧无 → 订阅（去重）。
    pub(crate) fn replace_deps(&mut self, owner: Owner, old: &[SignalId], new: &[SignalId]) {
        for id in old {
            if new.contains(id) {
                continue;
            }
            let empty = match self.dependents.get_mut(id) {
                Some(set) => {
                    set.retain(|existing| *existing != owner);
                    set.is_empty()
                }
                None => false,
            };
            if empty {
                self.dependents.remove(id);
            }
        }
        for id in new {
            if old.contains(id) {
                continue;
            }
            let set = self.dependents.entry(*id).or_default();
            if !set.contains(&owner) {
                set.push(owner);
            }
        }
    }

    /// 删一条边（`App::close_window` 的逐节点退订）。
    pub(crate) fn detach(&mut self, id: SignalId, owner: Owner) {
        let empty = match self.dependents.get_mut(&id) {
            Some(set) => {
                set.retain(|existing| *existing != owner);
                set.is_empty()
            }
            None => false,
        };
        if empty {
            self.dependents.remove(&id);
        }
    }

    /// 某个信号集合里是否存在**不在 `removed` 内**的节点读者；返回第一个（诊断用）。
    ///
    /// 消费者：`window_set::remove` 的「作用域归属」校验 —— `remove_node_with_scope` 要释放
    /// 子树的作用域时，若该作用域的存活信号仍被子树外的节点读取，释放必然破坏那个存活绑定
    /// （可证明的作用域错配），必须拒绝。
    pub(crate) fn node_reader_outside(
        &self,
        signals: &[SignalId],
        removed: &ahash::HashSet<NodeId>,
    ) -> Option<NodeId> {
        for signal in signals {
            let Some(owners) = self.dependents.get(signal) else {
                continue;
            };
            for owner in owners {
                if let Owner::Node { node, .. } = owner
                    && !removed.contains(node)
                {
                    return Some(*node);
                }
            }
        }
        None
    }

    /// 删除整条表项（空集合清理；`App::write` 的 `retain` 之后）。
    pub(crate) fn remove_signal(&mut self, id: SignalId) -> Option<OwnerSet> {
        self.dependents.remove(&id)
    }

    /// 当前有依赖边的信号数（仅测试：作用域释放「立即删边」的可测证据）。
    #[cfg(test)]
    pub(crate) fn signal_count(&self) -> usize {
        self.dependents.len()
    }
}

/// 一次 owner 求值内的「本帧读取集合」。内联 8 个；查重是线性（典型 1–5 个）。
pub(crate) struct ReadTracker {
    deps: SmallVec<[SignalId; 8]>,
}

impl ReadTracker {
    pub(crate) fn new() -> Self {
        Self {
            deps: SmallVec::new(),
        }
    }

    /// 去重登记（消费者：`ReadCtx::read`、`ReadCtx::read_derived`）。
    pub(crate) fn record(&mut self, id: SignalId) {
        if !self.deps.contains(&id) {
            self.deps.push(id);
        }
    }

    pub(crate) fn as_slice(&self) -> &[SignalId] {
        &self.deps
    }

    pub(crate) fn into_inner(self) -> SmallVec<[SignalId; 8]> {
        self.deps
    }
}

/// 派生求值栈（环检测）。
pub(crate) struct EvalStack {
    stack: SmallVec<[DerivedId; 8]>,
}

impl EvalStack {
    pub(crate) fn new() -> Self {
        Self {
            stack: SmallVec::new(),
        }
    }

    pub(crate) fn contains(&self, id: DerivedId) -> bool {
        self.stack.contains(&id)
    }

    pub(crate) fn push(&mut self, id: DerivedId) {
        self.stack.push(id);
    }

    pub(crate) fn pop(&mut self) -> Option<DerivedId> {
        self.stack.pop()
    }

    /// 帧边界兜底（防御上一帧被 `catch_unwind` 住的 panic 残留）。
    pub(crate) fn clear(&mut self) {
        self.stack.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

/// 入栈 + **`Drop` 出栈**：派生闭包 panic 时求值栈不会残留脏深度。
///
/// 拥有 `&mut EvalStack`；子 ctx 通过 [`EvalGuard::stack_mut`] 转借，因此借用检查也保证
/// 「guard 比子 ctx 活得久」（子 ctx 先 drop，guard 后 drop → `pop` 在其后执行）。
pub(crate) struct EvalGuard<'a> {
    stack: &'a mut EvalStack,
    id: DerivedId,
}

impl<'a> EvalGuard<'a> {
    /// 入栈并返回 guard（消费者：`ReadCtx::eval_derived`）。
    pub(crate) fn enter(stack: &'a mut EvalStack, id: DerivedId) -> Self {
        stack.push(id);
        Self { stack, id }
    }

    /// 把栈**转借**给子 ctx（消费者：`ReadCtx::eval_derived` 构造 nested ctx）。
    pub(crate) fn stack_mut(&mut self) -> &mut EvalStack {
        self.stack
    }
}

impl Drop for EvalGuard<'_> {
    fn drop(&mut self) {
        let popped = self.stack.pop();
        debug_assert_eq!(popped, Some(self.id), "eval stack is not LIFO");
    }
}

/// 显式读取上下文：**唯一**的读取入口，编译器保证「绕过登记」不可能。
///
/// 全部字段私有；只有引擎构造（`App::refresh_styles` 与嵌套求值）。
pub struct ReadCtx<'a> {
    values: &'a [SignalSlot],
    derived: &'a mut DerivedStore,
    graph: &'a mut DependencyGraph,
    /// 本 owner 的读取集合。**是借用不是自持有**：嵌套求值时换一个局部 tracker 即可。
    tracker: &'a mut ReadTracker,
    eval_stack: &'a mut EvalStack,
    owner: Owner,
}

impl<'a> ReadCtx<'a> {
    pub(crate) fn new(
        values: &'a [SignalSlot],
        derived: &'a mut DerivedStore,
        graph: &'a mut DependencyGraph,
        tracker: &'a mut ReadTracker,
        eval_stack: &'a mut EvalStack,
        owner: Owner,
    ) -> Self {
        Self {
            values,
            derived,
            graph,
            tracker,
            eval_stack,
            owner,
        }
    }

    /// 本 owner 的 `Owner`（`replace_deps` 的键；消费者：`App::refresh_styles`）。
    pub(crate) fn owner(&self) -> Owner {
        self.owner
    }

    /// 读普通信号：登记依赖 + 返回**读守卫**（`Rc` 递增，不克隆值）。
    ///
    /// 返回的 [`SignalRef`] 不借用 `self`：同一个 `ReadCtx` 上可以连续取多个守卫，
    /// 守卫也可以在 `ReadCtx` 销毁后继续存活（只冻结被读的那一格值）。
    ///
    /// # Panics
    /// - 句柄已失效（作用域已释放 / 槽位已回收复用）：**确定性失败**，绝不返回复用后的新值
    ///   （见 `node::signal` 模块文档〈作用域回收与句柄失效〉）；
    /// - `SignalSlot` 的不变量断言（槽位类型与 `T` 不一致）。
    pub fn read<T: 'static>(&mut self, signal: &Signal<T>) -> SignalRef<T> {
        let id = signal.id();
        // 先校验代际再登记依赖：失效句柄必须在 **panic 之前**就被拦住，不能留下一条死边。
        let slot = self
            .values
            .get(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
            .live(id);
        self.tracker.record(id);
        slot.read_owned(id)
    }

    /// 读瞬态值：**不登记**依赖。
    ///
    /// # Panics
    /// `SignalSlot` 的不变量断言 / 句柄已失效。
    pub fn read_transient<T: 'static>(&mut self, transient: &Transient<T>) -> SignalRef<T> {
        let id = transient.id();
        self.slot(id).read_transient(id)
    }

    /// 读派生值：登记依赖 + 惰性求值 + 返回缓存快照守卫。
    ///
    /// # Errors
    /// `EngineError::CyclicDerived { derived }`：求值栈上再次遇到同一个 `DerivedId`（真环）。
    /// 错误路径**不写缓存、不清 `dirty`**。
    ///
    /// # Panics
    /// 缓存/闭包的类型与 `T` 不符（不变量破坏）；派生闭包自身 panic 会原样向上传播 ——
    /// 但闭包会在展开时放回（R03-12），下一次读会**重试**该闭包，不会把派生永久 brick。
    pub fn read_derived<T: 'static>(
        &mut self,
        derived: &Derived<T>,
    ) -> Result<SignalRef<T>, EngineError> {
        let id = derived.id();
        let handle = self.derived.handle(id);
        self.tracker.record(handle);
        if self.derived.is_dirty(id) {
            self.eval_derived(id)?;
        }
        Ok(self.derived.read_value::<T>(id))
    }

    /// 诊断/测试专用：读值但**不登记**依赖（仍返回读守卫）。
    #[cfg(test)]
    #[must_use]
    pub fn peek<T: 'static>(&self, signal: &Signal<T>) -> SignalRef<T> {
        self.slot(signal.id()).read_owned(signal.id())
    }

    fn slot(&self, id: SignalId) -> &SignalSlot {
        self.values
            .get(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
            .live(id)
    }

    /// 求值一个派生值（环检测 + `EvalGuard` + 失败不污染缓存 + panic 安全放回闭包）。
    ///
    /// 闭包暂时 `take` 出去是借用约束决定的（求值期要把 `&mut DerivedStore` 交给 `ReadCtx`）；
    /// 但 `Fn` 闭包会在**任何**退出路径上放回 —— 包括应用闭包 panic：见下面的 `catch_unwind`。
    fn eval_derived(&mut self, id: DerivedId) -> Result<(), EngineError> {
        if self.eval_stack.contains(id) {
            return Err(EngineError::CyclicDerived { derived: id });
        }
        let mut guard = EvalGuard::enter(&mut *self.eval_stack, id);

        let Some(compute) = self.derived.take_compute(id) else {
            // 不变量破坏：闭包只在本函数求值期间被 `take`，而求值期不重入（上面的环检测先拦）。
            // 保留 `CyclicDerived` 作为**最后防线**（panic 之外不再有合法路径走到这里），
            // 但不作为"某派生已经坏了"的判据（R03-12 的伪装面已消除）。
            debug_assert!(
                false,
                "derived compute closure is missing outside evaluation"
            );
            return Err(EngineError::CyclicDerived { derived: id });
        };

        let mut nested = ReadTracker::new();
        // R03-12：应用闭包 panic 时，旧实现让 `put_compute` 永远执行不到 ⇒ 该派生**永久 brick**，
        // 且下一次读被伪装成 `CyclicDerived`（"存在环"）。这里捕获 unwind、把闭包放回、再把
        // payload 原样继续展开：panic 仍然是 panic（对外语义不变），但引擎状态自愈 —— 下一次
        // 读会重试闭包，而不是把应用闭包的一次 bug 升级成该派生值的水久失效。
        // 闭包已在 `EvalGuard` 的作用域内取走，`catch_unwind` 只包住调用本身。
        //
        // 适用面：`[profile.release] panic = "abort"`（workspace 根 `Cargo.toml`）时进程直接终止，
        // 不存在"brick"；本保障针对 unwind 构建（debug / test / 任何改回 unwind 的 profile）。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut nested_ctx = ReadCtx {
                values: self.values,
                derived: &mut *self.derived,
                graph: &mut *self.graph,
                tracker: &mut nested,
                eval_stack: guard.stack_mut(),
                owner: Owner::Derived(id),
            };
            compute(&mut nested_ctx)
        }));
        self.derived.put_compute(id, compute);
        let value: Box<dyn Any> = match outcome {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        };

        self.derived.store_value(id, Rc::from(value));

        let old = self.derived.deps(id);
        self.graph
            .replace_deps(Owner::Derived(id), old, nested.as_slice());
        self.derived.set_deps(id, nested.into_inner());

        drop(guard);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{DependencyGraph, EvalStack, Owner, OwnerSet, ReadCtx, ReadTracker};
    use crate::{
        node::{
            id::NodeId,
            signal::{DerivedId, SignalId},
        },
        window::WindowId,
    };

    fn node_owner(index: u32) -> Owner {
        Owner::Node {
            window: WindowId::for_test(1),
            node: NodeId::for_test(index, 1),
        }
    }

    fn signal(index: u32) -> SignalId {
        SignalId::for_test(index)
    }

    #[test]
    fn read_tracker_dedupes_and_keeps_first_order() {
        let mut tracker = ReadTracker::new();
        tracker.record(signal(3));
        tracker.record(signal(1));
        tracker.record(signal(3));

        assert_eq!(tracker.as_slice(), &[signal(3), signal(1)]);
        assert_eq!(tracker.into_inner().len(), 2);
    }

    #[test]
    fn replace_deps_adds_new_edges() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        graph.replace_deps(owner, &[], &[signal(0), signal(1)]);

        assert_eq!(
            graph.dependents_mut(signal(0)).map(|set| set.as_slice()),
            Some([owner].as_slice())
        );
        assert_eq!(
            graph.dependents_mut(signal(1)).map(|set| set.as_slice()),
            Some([owner].as_slice())
        );
    }

    #[test]
    fn replace_deps_removes_missing_edges() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        graph.replace_deps(owner, &[], &[signal(0), signal(1)]);
        graph.replace_deps(owner, &[signal(0), signal(1)], &[signal(1)]);

        assert!(
            graph.dependents_mut(signal(0)).is_none(),
            "空集合必须删除表项"
        );
        assert!(graph.dependents_mut(signal(1)).is_some());
    }

    #[test]
    fn replace_deps_is_idempotent() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        graph.replace_deps(owner, &[], &[signal(0)]);
        graph.replace_deps(owner, &[signal(0)], &[signal(0)]);

        let set = graph.dependents_mut(signal(0)).expect("表项存在");
        assert_eq!(set.len(), 1, "同一 owner 只能有一份");
    }

    #[test]
    fn replace_deps_replaces_not_accumulates() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        // 条件分支：第一次读 a，第二次读 b → 最终只依赖 b。
        graph.replace_deps(owner, &[], &[signal(0)]);
        graph.replace_deps(owner, &[signal(0)], &[signal(1)]);

        assert!(graph.dependents_mut(signal(0)).is_none());
        assert!(graph.dependents_mut(signal(1)).is_some());
    }

    #[test]
    fn detach_removes_edge_and_entry() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);
        graph.replace_deps(owner, &[], &[signal(0)]);

        graph.detach(signal(0), owner);

        assert!(
            graph.dependents_mut(signal(0)).is_none(),
            "最后一条边删除后表项必须消失"
        );
        assert!(graph.remove_signal(signal(0)).is_none());
    }

    #[test]
    fn owner_set_inline_capacity() {
        let mut set = OwnerSet::new();
        set.push(node_owner(0));
        set.push(node_owner(1));
        assert!(!set.spilled(), "2 个 owner 必须内联");
        set.push(node_owner(2));
        assert!(set.spilled(), "第 3 个 owner 才溢出到堆");
    }

    #[test]
    fn eval_stack_push_pop_and_contains() {
        let mut stack = EvalStack::new();
        let id = DerivedId::for_test(0);

        assert!(stack.is_empty());
        stack.push(id);
        assert!(stack.contains(id));
        assert!(!stack.is_empty());
        assert_eq!(stack.pop(), Some(id));
        assert!(stack.is_empty());
    }

    #[test]
    fn peek_reads_without_tracking() {
        // `peek` 只在 test 构建存在：构造一个最小 ReadCtx 验证它不登记依赖。
        let mut store = crate::node::signal::StateStore::with_capacity(1, 0);
        let id = store.push_owned(7u32);
        let (values, derived) = store.split();
        let graph = &mut DependencyGraph::new();
        let mut tracker = ReadTracker::new();
        let mut eval_stack = EvalStack::new();
        let signal = crate::node::signal::Signal::<u32>::new(id);
        let ctx = ReadCtx::new(
            values,
            derived,
            graph,
            &mut tracker,
            &mut eval_stack,
            node_owner(0),
        );

        assert_eq!(*ctx.peek(&signal), 7);
        assert!(tracker.as_slice().is_empty(), "peek 不得登记依赖");
    }
}

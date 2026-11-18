//! 状态模型：`Signal`/`Derived`/`Transient` 句柄 + `StateStore`（类型擦除的值存储）。
//!
//! 设计见 `design/phase-03-node-state.md` §7。要点：
//! - 句柄是 `Copy`（`PhantomData<fn() -> T>`，不要求 `T: Copy`），值只存在 `StateStore`；
//! - `SignalId`/`DerivedId` 是**稠密索引**，条目永不删除 ⇒ 无代际、无自由表；
//! - 派生值**惰性**求值、push 失效；环检测在运行时求值栈上（见 `node::dependency::EvalStack`）；
//! - **没有版本号**：失效是 push 模型，等值短路由 `App::write` 的 `PartialEq` 完成（S-27 / S-35）。
//!
//! # 读是借用，不是拷贝（R03-4）
//!
//! 所有公开读入口（`ReadCtx::read` / `App::read` / `ActionCtx::read`）返回 [`SignalRef<T>`]：
//! 一个 `Deref<Target = T>` 的**读守卫**，值为 `Rc<T>`（引用计数递增，不克隆值本身）。
//!
//! ```
//! # use ui_core::app::App;
//! # fn demo(app: &mut App) {
//! # let price = app.signal(100.0f32);
//! // 读：零拷贝借用
//! assert_eq!(*app.read(&price), 100.0);
//! // 写：等值短路 + 依赖失效
//! app.write(&price, 101.0);
//! # }
//! ```
//!
//! ## 借用规则（一次说清）
//!
//! 1. **读守卫与句柄同生共死**：`SignalRef` 持有 `Rc<T>` 的强引用，因此**不借用** `App` /
//!    `ReadCtx`。它可以在读取点之后继续存活（跨语句、跨 `App` 的可变借用），代价是**该信号
//!    的值被冻结**：任何对**同一个**信号的写（`App::write` / `App::update` /
//!    `write_transient`）在守卫存活期间都会 **panic**，并在 panic 消息里给出 `SignalId` 与
//!    守卫数量。这与 GPUI 的「借用冲突显式失败」、Leptos `ReadGuard` 的「写时已借用则 panic」
//!    是同一策略：**不静默拷贝出旧值**，也不写丢更新。
//! 2. **粒度是单个信号**：读 A 的守卫不会阻止写 B（不同信号互不干扰）。
//! 3. **守卫不可 `Clone`**：需要长期持有的旧值请显式 `(*guard).clone()` —— 那是一次**可见的**
//!    深拷贝，而不是读路径的隐式成本。
//! 4. **派生值**（`Derived`）的读守卫是**缓存快照**：重新求值只会替换缓存里的 `Rc`，
//!    已发出的守卫继续指向旧值（快照语义），因此派生读**不会**阻塞写上游。
//! 5. **作用域回收**（[`SignalScope`]）：作用域释放后，其信号的写是 **no-op**（返回 `false`、
//!    不标脏、不请求帧）；读仍返回冻结值（值在内存里，但不再变化）。

use std::{any::Any, fmt, marker::PhantomData, ops::Deref, rc::Rc};

use smallvec::SmallVec;

use crate::node::dependency::ReadCtx;

/// 信号 / 瞬态值的稠密索引（`StateStore::values` 的下标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SignalId(u32);

impl SignalId {
    /// 下标（诊断/依赖集合用）。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    /// `u64` 投影（结构化日志，设计 §9.1）。
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        self.0 as u64
    }

    /// 仅测试：直接构造（不进入非 test 构建，因此没有 `dead_code`）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32) -> Self {
        Self(index)
    }
}

/// 派生值的稠密索引（`DerivedStore::slots` 的下标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DerivedId(u32);

impl DerivedId {
    /// 下标。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    /// `u64` 投影（`EngineError::CyclicDerived` 的诊断字段）。
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        self.0 as u64
    }

    /// 仅测试：直接构造。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32) -> Self {
        Self(index)
    }
}

/// 普通信号句柄：只有 id，值在 `StateStore`（设计 §3.2「节点不持有值」）。
///
/// `Copy` 无条件成立：`PhantomData<fn() -> T>` 不要求 `T: Copy`，也不携带 drop-check/auto-trait 负担。
pub struct Signal<T> {
    id: SignalId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Signal<T> {}

impl<T> std::fmt::Debug for Signal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signal(#{})", self.id.index())
    }
}

impl<T> PartialEq for Signal<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for Signal<T> {}

impl<T> Signal<T> {
    /// crate 内构造（消费者：`App::signal`）。
    pub(crate) const fn new(id: SignalId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    /// crate 内 id（消费者：`App::write`、`ReadCtx::read`）。
    pub(crate) const fn id(self) -> SignalId {
        self.id
    }
}

/// 派生值句柄（惰性求值，§7.4）。
pub struct Derived<T> {
    id: DerivedId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Derived<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Derived<T> {}

impl<T> std::fmt::Debug for Derived<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Derived(#{})", self.id.index())
    }
}

impl<T> Derived<T> {
    /// crate 内构造（消费者：`App::derived`）。
    pub(crate) const fn new(id: DerivedId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    /// crate 内 id（消费者：`App` 的注册、`ReadCtx::read_derived`）。
    pub(crate) const fn id(self) -> DerivedId {
        self.id
    }
}

/// 瞬态值句柄（滚动偏移/悬浮态，§3.7）：读不登记、写不失效。
pub struct Transient<T> {
    id: SignalId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Transient<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Transient<T> {}

impl<T> std::fmt::Debug for Transient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Transient(#{})", self.id.index())
    }
}

impl<T> Transient<T> {
    /// crate 内构造（消费者：`App::transient`）。
    pub(crate) const fn new(id: SignalId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    /// crate 内 id（消费者：`App::write_transient`、`ReadCtx::read_transient`）。
    pub(crate) const fn id(self) -> SignalId {
        self.id
    }
}

/// 读守卫：[`ReadCtx::read`](crate::node::dependency::ReadCtx::read) /
/// [`App::read`](crate::app::App::read) / [`ActionCtx::read`](crate::interaction::ActionCtx::read)
/// 的返回值。
///
/// 语义（完整借用规则见模块文档）：
/// - `Deref<Target = T>`：读路径**零拷贝**（值是 `Rc<T>`，读只做一次引用计数递增 + 类型下行）；
/// - **不借用** `App` / `ReadCtx`：守卫活过读取语句是合法的，越过它的生命周期使用也合法 ——
///   只要期间没人写**同一个**信号；对同一信号的写会 panic（可诊断），而不是静默拷贝旧值；
/// - 不可 `Clone`：长期持有旧值必须显式 `(*guard).clone()`（一次可见的深拷贝）。
///
/// 守卫的存活不阻止其它信号被写、不阻止布局/绘制推进 —— 它只冻结被读的那一格值。
///
/// 守卫是**临时量**，它的寿命由 Rust 的 terminating scope 规则决定：`match` 头与 `let` 绑定
/// 都会把它留到写的那一刻（分别活到整个 `match` 结束 / 作用域结束），而 `if` 条件不会。
/// 完整的对照与安全写法见下文〈三种自然写法与 `match` 陷阱〉；消灭陷阱的组合子是
/// [`App::update_with`](crate::app::App::update_with)。
///
/// # 三种自然写法与 `match` 陷阱
///
/// 对同一个信号「先读后写」，三种看起来同样自然的写法里有两种会 panic —— 区别只在读守卫
/// 这个临时量活到哪一行：
///
/// ```text
/// // ✅ 安全：`if` 条件是 terminating scope —— 条件里的临时守卫在**进入 body 之前**析构。
/// if *cx.read(&s) > 0 {
///     cx.update(&s, |v| *v += 1);          // 此刻没有活跃守卫 ⇒ 不 panic
/// }
///
/// // ❌ panic：`match` 的 scrutinee **不是** terminating scope，它的临时量活到整个 `match`
/// //    结束（含所有分支）—— 分支里的写撞上仍然存活的读守卫。
/// match *cx.read(&s) {
///     Mode::Idle => cx.write(&s, Mode::Busy),   // panic: SignalRef read guard alive
///     _ => {}
/// }
///
/// // ❌ panic：`let` 绑定把守卫的寿命延长到作用域结束（模块文档的借用规则 1，文档化语义）。
/// let guard = cx.read(&s);
/// cx.write(&s, Mode::Busy);                 // panic
/// ```
///
/// `match` 的两个**正确**写法（枚举状态机的自然写法都在里面）：
///
/// ```text
/// // 1) 组合子：把整个 match 搬进 `update_with` 的闭包。闭包拿到 `&mut T`、拿不到 `&mut App`，
/// //    在一次借用里完成「看旧值 ⇒ 决定新值」，因此连读守卫都不存在 —— 这是消灭 match 陷阱
/// //    的写法。
/// //    注意：把 `update_with` 塞进**外层** `match *cx.read(&s)` 的分支里仍然会 panic ——
/// //    陷阱的根因是外层读守卫，不是写入口的 API。
/// cx.update_with(&s, |mode| match *mode {
///     Mode::Idle => *mode = Mode::Busy,
///     _ => {}
/// });
///
/// // 2) 在进入 `match` **之前**把需要的值取出来：`*cx.read(&s)` 是 Copy 出去（不是借用），
/// //    守卫在那条语句结束时析构。
/// let mode = *cx.read(&s);
/// match mode {
///     Mode::Idle => { cx.write(&s, Mode::Busy); }   // 没有活跃守卫 ⇒ 不 panic
///     _ => {}
/// }
/// ```
///
/// ## 为什么编译器阻止不了
///
/// `SignalRef<T>` 只持有一个 `Rc<T>`，**不借用** `App` / `ReadCtx`：`cx.read(&s)` 返回之后
/// `cx` 的借用就结束了。于是 borrowck 看到的是「一个指向堆上值的不可变借用（守卫）」加上
/// 「一个 `&mut App` / `&mut ReadCtx`」两个**互不相干**的合法借用，两者之间没有任何生命周期
/// 相连 —— 上面那段 `match` 在编译期无懈可击。
///
/// 真正的冲突是**运行期别名**：同一个信号槽的值被读守卫与就地写同时握住。引擎明确拒绝写时
/// 复制（`Rc::make_mut`）—— 那等于让守卫静默拿到旧值快照，与模块文档「读是借用、不静默拷贝」
/// 的取向相反。所以唯一可行的检测点是写入口：用 `Rc::strong_count` 判「除 `StateStore` 之外
/// 还有谁持有这个值」，`> 0` 即 panic 并给出 `SignalId` 与守卫数量。该判据的前提是
/// 「`StateStore` 是唯一持有者，只有读守卫会克隆那个 `Rc`」—— 将来任何内部缓存共享同一个
/// `Rc`，都会被误判成活跃守卫（AR4 §2.1 的脆弱点）。
pub struct SignalRef<T> {
    value: Rc<T>,
}

impl<T: 'static> SignalRef<T> {
    /// 由类型擦除的 `Rc<dyn Any>` 下行构造（类型不符 = 引擎不变量破坏，panic 带 `SignalId`）。
    pub(crate) fn from_any(value: Rc<dyn Any>, id: u64) -> Self {
        match value.downcast::<T>() {
            Ok(value) => Self { value },
            Err(_) => panic!(
                "signal #{id}: store value type mismatch — engine invariant broken \
                 (SignalId ↔ T is a constructive bijection)"
            ),
        }
    }
}

impl<T> AsRef<T> for SignalRef<T> {
    fn as_ref(&self) -> &T {
        &self.value
    }
}

impl<T> Deref for SignalRef<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for SignalRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SignalRef").field(&*self.value).finish()
    }
}

impl<T: fmt::Display> fmt::Display for SignalRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.value, f)
    }
}

/// 信号作用域：把一组信号的**生命周期**绑定到一个应用级所有者（典型是窗口 / 面板）。
///
/// 用法：
/// ```text
/// let scope = app.signal_scope();              // 应用自建作用域（面板/对话框）
/// let price = app.signal_scoped(scope, 0.0);   // 该作用域内的信号
/// ...
/// app.release_scope(scope);                    // 作用域释放：之后的写是 no-op
/// ```
///
/// 释放语义（[`App::release_scope`](crate::app::App::release_scope)）：
/// 1. 作用域内信号的**写**变成 no-op（`write` 返回 `false`，不标脏、不唤醒 —— 不会有僵尸唤醒）；
/// 2. 依赖图里以这些信号为键的边被**立即删除**（不是"等下次写时惰性清理"），
///    因此释放后 `dependency_dump` 里查不到它们、`App::write` 也不再遍历任何 owner；
/// 3. 值仍驻留（稠密 id 表不回收槽位），读返回冻结值；`App::signal` 注册的根作用域信号永不被释放。
///
/// 窗口作用域由引擎自动管理：`App::create_window` 建作用域，
/// [`App::window_signal`](crate::app::App::window_signal) 往里注册信号，
/// `App::close_window` 自动释放（见 `app::window_set`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SignalScope {
    /// 作用域下标；0 = 根作用域（永不释放，`App::signal` 的归属）。
    index: u32,
}

impl SignalScope {
    /// 根作用域（`App::signal` 的归属；不向应用暴露句柄 —— 根作用域没有释放入口）。
    pub(crate) const ROOT: Self = Self { index: 0 };

    /// crate 内构造（`StateStore::new_scope`）。
    pub(crate) const fn from_index(index: u32) -> Self {
        Self { index }
    }

    /// 作用域下标（诊断用；0 = 根作用域）。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }
}

/// 信号表的一格：普通信号 / 派生句柄 / 瞬态值。
pub(crate) enum SignalValue {
    /// 普通信号：类型擦除的值（`Rc` 共享给读守卫；`strong_count == 1` 时才可原地写）。
    Owned(Rc<dyn Any>),
    /// 派生值的**句柄槽**：值在 `DerivedSlot::cache`。
    Derived(DerivedId),
    /// 瞬态值：类型擦除的值；读不登记、写不失效。
    Transient(Rc<dyn Any>),
}

/// 信号表的一格（**没有**版本号：S-27）。
pub(crate) struct SignalSlot {
    value: SignalValue,
    /// 归属作用域（`SignalScope::ROOT.index` = 根作用域）。
    scope: u32,
}

impl SignalSlot {
    /// `SignalValue::Owned` 的借用视图（不克隆、不产生守卫）。
    ///
    /// 消费者：`App::write` 的等值短路（借用比较）。
    #[inline]
    pub(crate) fn owned_ref<T: 'static>(&self) -> Option<&T> {
        match &self.value {
            SignalValue::Owned(value) => value.downcast_ref::<T>(),
            SignalValue::Derived(_) | SignalValue::Transient(_) => None,
        }
    }

    /// `SignalValue::Transient` 的借用视图（不克隆、不产生守卫）。
    pub(crate) fn transient_ref<T: 'static>(&self) -> Option<&T> {
        match &self.value {
            SignalValue::Transient(value) => value.downcast_ref::<T>(),
            SignalValue::Owned(_) | SignalValue::Derived(_) => None,
        }
    }

    /// 读守卫：`Rc` 引用计数递增 + 类型下行（不克隆值）。
    ///
    /// # Panics
    /// 槽位不是普通信号，或其内部类型与 `T` 不一致（引擎不变量被破坏）。
    pub(crate) fn read_owned<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        match &self.value {
            SignalValue::Owned(value) => SignalRef::from_any(Rc::clone(value), id.to_bits()),
            SignalValue::Derived(derived) => panic!(
                "signal #{}: slot is the handle of derived {derived:?}, not a plain signal",
                id.index()
            ),
            SignalValue::Transient(_) => {
                panic!("signal #{}: slot is a transient value", id.index())
            }
        }
    }

    /// 瞬态读守卫。
    ///
    /// # Panics
    /// 槽位不是瞬态值，或其内部类型与 `T` 不一致。
    pub(crate) fn read_transient<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        match &self.value {
            SignalValue::Transient(value) => SignalRef::from_any(Rc::clone(value), id.to_bits()),
            SignalValue::Owned(_) | SignalValue::Derived(_) => {
                panic!("signal #{}: slot is not a transient value", id.index())
            }
        }
    }

    /// 覆盖普通信号的值：零守卫时**原地**写（复用分配），否则 panic（借用冲突）。
    ///
    /// # Panics
    /// - 存在活跃 [`SignalRef`]：`strong_count > 1`（读守卫持有同一个 `Rc`）；
    /// - 槽位不是普通信号（`write_transient` 误用防护）。
    #[inline]
    pub(crate) fn write_owned<T: 'static>(&mut self, id: SignalId, value: T) {
        let SignalValue::Owned(existing) = &mut self.value else {
            panic!(
                "signal #{}: write() targets a transient/derived slot — use the matching entry point",
                id.index()
            );
        };
        let guards = Rc::strong_count(existing) - 1;
        if guards > 0 {
            panic!(
                "signal #{}: write while {guards} SignalRef read guard(s) are alive — \
                 the engine does not copy values on read, so an in-place write would alias them. \
                 Drop the guard(s) first (e.g. `let value = (*cx.read(&s)).clone();`)",
                id.index()
            );
        }
        if let Some(current) = Rc::get_mut(existing).and_then(|any| any.downcast_mut::<T>()) {
            // 同类型复用已有分配：HUD/动画每帧写同一批信号，避免每帧一次堆分配（F-214）。
            *current = value;
            return;
        }
        *existing = Rc::new(value);
    }

    /// 原地更新普通信号（无等值短路：调用方声明了「这次修改一定要生效」），返回闭包的值。
    ///
    /// # Panics
    /// 同 [`SignalSlot::write_owned`]。
    pub(crate) fn update_owned<T: 'static, R>(
        &mut self,
        id: SignalId,
        update: impl FnOnce(&mut T) -> R,
    ) -> R {
        let SignalValue::Owned(existing) = &mut self.value else {
            panic!(
                "signal #{}: update() targets a transient/derived slot",
                id.index()
            );
        };
        let guards = Rc::strong_count(existing) - 1;
        if guards > 0 {
            panic!(
                "signal #{}: update while {guards} SignalRef read guard(s) are alive — \
                 drop the guard(s) first",
                id.index()
            );
        }
        match Rc::get_mut(existing).and_then(|any| any.downcast_mut::<T>()) {
            Some(current) => update(current),
            None => panic!(
                "signal #{}: value type mismatch — engine invariant broken",
                id.index()
            ),
        }
    }

    /// 覆盖瞬态值（原地复用；与 `write_owned` 同样要求零守卫）。
    ///
    /// # Panics
    /// 存在活跃 [`SignalRef`]，或槽位不是瞬态值。
    pub(crate) fn write_transient<T: 'static>(&mut self, id: SignalId, value: T) {
        let SignalValue::Transient(existing) = &mut self.value else {
            panic!(
                "signal #{}: write_transient() targets a plain signal — use write()",
                id.index()
            );
        };
        let guards = Rc::strong_count(existing) - 1;
        if guards > 0 {
            panic!(
                "signal #{}: transient write while {guards} SignalRef read guard(s) are alive",
                id.index()
            );
        }
        if let Some(current) = Rc::get_mut(existing).and_then(|any| any.downcast_mut::<T>()) {
            *current = value;
            return;
        }
        *existing = Rc::new(value);
    }
}

/// 派生槽：闭包 + 缓存 + 脏位 + 上次依赖。
pub(crate) struct DerivedSlot {
    /// 该派生值在信号表里的句柄槽 id（依赖图只用 `SignalId` 作键）。
    handle: SignalId,
    /// 计算闭包。求值期 `take()` 出去（避免与 `&mut self` 别名），求值后放回。
    compute: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>>,
    /// 缓存值（`Some` 且 `dirty == false` 时有效）。
    ///
    /// `Rc` 而不是 `Box`：读守卫是缓存值的**快照**（重新求值只替换这里的 `Rc`，
    /// 已发出的守卫继续指向旧值），因此派生读不需要深拷贝也不需要写冲突检查。
    cache: Option<Rc<dyn Any>>,
    /// 失效标志（push 模型）。
    dirty: bool,
    /// 上次求值登记的依赖（差分清理用）。与 `ReadTracker` 同容量（B1）。
    deps: SmallVec<[SignalId; 8]>,
}

/// 派生表的唯一持有者：封装 id→槽位映射与 `Option` 不变量。
pub(crate) struct DerivedStore {
    slots: Vec<DerivedSlot>,
}

impl DerivedStore {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// 登记一个新派生（`handle` 已由 `StateStore::push_derived` 铸好）。
    pub(crate) fn push(
        &mut self,
        handle: SignalId,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) {
        self.slots.push(DerivedSlot {
            handle,
            compute: Some(compute),
            cache: None,
            dirty: true,
            deps: SmallVec::new(),
        });
    }

    pub(crate) fn is_dirty(&self, id: DerivedId) -> bool {
        self.slot(id).dirty
    }

    /// 句柄 id（消费者：`ReadCtx::read_derived` 的依赖登记）。
    pub(crate) fn handle(&self, id: DerivedId) -> SignalId {
        self.slot(id).handle
    }

    /// 取走闭包（求值期）。`None` = 该派生正在求值 / 上次求值 panic 未放回。
    pub(crate) fn take_compute(
        &mut self,
        id: DerivedId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>> {
        self.slot_mut(id).compute.take()
    }

    pub(crate) fn put_compute(
        &mut self,
        id: DerivedId,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) {
        self.slot_mut(id).compute = Some(compute);
    }

    /// 写缓存并清脏位（**只有成功求值才调用**，见 §7.4 的「失败不污染缓存」）。
    pub(crate) fn store_value(&mut self, id: DerivedId, value: Rc<dyn Any>) {
        let slot = self.slot_mut(id);
        slot.cache = Some(value);
        slot.dirty = false;
    }

    /// 读缓存守卫（`Rc` 递增 + 类型下行；**不**克隆值）。
    ///
    /// # Panics
    /// 尚未求值（调用方保证先检查 `is_dirty`），或类型不一致（不变量破坏）。
    pub(crate) fn read_value<T: 'static>(&self, id: DerivedId) -> SignalRef<T> {
        let cache = self
            .slot(id)
            .cache
            .as_ref()
            .expect("derived value has not been evaluated");
        SignalRef::from_any(Rc::clone(cache), id.to_bits())
    }

    pub(crate) fn deps(&self, id: DerivedId) -> &[SignalId] {
        &self.slot(id).deps
    }

    pub(crate) fn set_deps(&mut self, id: DerivedId, deps: SmallVec<[SignalId; 8]>) {
        self.slot_mut(id).deps = deps;
    }

    /// 置脏；返回「本次由 clean 变 dirty」（级联只传播一次）。
    pub(crate) fn mark_dirty(&mut self, id: DerivedId) -> bool {
        let slot = self.slot_mut(id);
        if slot.dirty {
            false
        } else {
            slot.dirty = true;
            true
        }
    }

    fn slot(&self, id: DerivedId) -> &DerivedSlot {
        self.slots
            .get(id.index() as usize)
            .expect("derived id out of range: engine invariant broken")
    }

    fn slot_mut(&mut self, id: DerivedId) -> &mut DerivedSlot {
        self.slots
            .get_mut(id.index() as usize)
            .expect("derived id out of range: engine invariant broken")
    }
}

/// 信号 / 派生 / 瞬态值的唯一存储 + 信号作用域表。
pub(crate) struct StateStore {
    values: Vec<SignalSlot>,
    derived: DerivedStore,
    /// 作用域表；下标 0 = 根作用域（永不释放）。
    scopes: Vec<ScopeSlot>,
    /// 已释放作用域数。0 = **从未释放过任何作用域** ⇒ 写路径可以完全跳过作用域检查
    /// （常见情形：应用只开窗、不关窗）。窗口关闭后它变成 1 并保持 > 0，写路径恢复逐槽检查。
    released_scopes: usize,
}

/// 一个作用域的运行时状态。
struct ScopeSlot {
    /// 是否已释放（释放后其信号的写是 no-op，依赖边被立即删除）。
    released: bool,
    /// 该作用域铸造的全部信号 id（释放时逐条清依赖边）。
    signals: Vec<SignalId>,
}

impl StateStore {
    pub(crate) fn with_capacity(signals: usize, derived: usize) -> Self {
        Self {
            values: Vec::with_capacity(signals),
            derived: DerivedStore::with_capacity(derived),
            scopes: vec![ScopeSlot {
                released: false,
                signals: Vec::new(),
            }],
            released_scopes: 0,
        }
    }

    /// 拆分借用：信号表只读 + 派生表可变（同一结构体的不交叠字段）。
    pub(crate) fn split(&mut self) -> (&[SignalSlot], &mut DerivedStore) {
        (&self.values, &mut self.derived)
    }

    /// 新建一个作用域（`App::signal_scope` / `App::create_window`）。
    pub(crate) fn new_scope(&mut self) -> SignalScope {
        let index = u32::try_from(self.scopes.len()).expect("信号作用域溢出 u32");
        self.scopes.push(ScopeSlot {
            released: false,
            signals: Vec::new(),
        });
        SignalScope::from_index(index)
    }

    /// 释放作用域并返回它的全部信号 id（调用方负责清依赖图里以它们为键的边）。
    ///
    /// 重复释放返回空切片（幂等）。根作用域不可释放（`None`）。
    pub(crate) fn release_scope(&mut self, scope: SignalScope) -> Option<Vec<SignalId>> {
        let slot = self.scopes.get_mut(scope.index() as usize)?;
        if slot.released {
            return Some(Vec::new());
        }
        slot.released = true;
        self.released_scopes += 1;
        Some(std::mem::take(&mut slot.signals))
    }

    pub(crate) fn push_owned<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_owned_scoped(SignalScope::ROOT, value)
    }

    /// 在指定作用域里注册普通信号。
    pub(crate) fn push_owned_scoped<T: 'static>(
        &mut self,
        scope: SignalScope,
        value: T,
    ) -> SignalId {
        self.push_slot(SignalValue::Owned(Rc::new(value)), scope)
    }

    pub(crate) fn push_transient<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_slot(SignalValue::Transient(Rc::new(value)), SignalScope::ROOT)
    }

    pub(crate) fn push_derived(
        &mut self,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) -> (DerivedId, SignalId) {
        let derived_id = DerivedId(u32::try_from(self.derived.len()).expect("派生槽位溢出 u32"));
        let handle = self.push_slot(SignalValue::Derived(derived_id), SignalScope::ROOT);
        self.derived.push(handle, compute);
        (derived_id, handle)
    }

    /// 覆盖普通信号的值（**不**比较、**不**失效：失效由 `App::write` 负责）。
    ///
    /// # Panics
    /// 槽位不是普通信号，或该信号存在活跃读守卫（见 [`SignalSlot::write_owned`]）。
    #[cfg(test)]
    pub(crate) fn set_owned<T: 'static>(&mut self, id: SignalId, value: T) {
        self.slot_mut(id).write_owned(id, value);
    }

    /// **写路径的合并入口**（一次槽位查找）：作用域检查 → 等值短路 → 借用冲突检查 → 写。
    ///
    /// 返回值：`None` = 作用域已释放（no-op，调用方不失效、不唤醒）；`Some(false)` = 等值短路；
    /// `Some(true)` = 已写入（调用方负责 `invalidate`）。
    ///
    /// 为什么合并：`App::write` 是最高频热路径（行情推送每帧成百上千次），三次
    /// `Vec` 索引 + 两次作用域表的 Option 链在基准里是可见成本。
    #[inline]
    pub(crate) fn write_owned<T: PartialEq + 'static>(
        &mut self,
        id: SignalId,
        value: T,
    ) -> Option<bool> {
        let StateStore {
            values,
            scopes,
            released_scopes,
            ..
        } = self;
        let slot = values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        // 快路径：从未释放过作用域 ⇒ 不可能有"已释放作用域的信号"（省掉一次 Option 链）。
        if *released_scopes > 0
            && scopes
                .get(slot.scope as usize)
                .is_some_and(|scope| scope.released)
        {
            return None;
        }
        if slot.owned_ref::<T>() == Some(&value) {
            return Some(false);
        }
        slot.write_owned(id, value);
        Some(true)
    }

    /// 原地更新普通信号（无等值短路；与 [`StateStore::write_owned`] 同样一次查找）。
    ///
    /// 返回值：`None` = 作用域已释放（no-op，闭包不会被调用）；`Some(r)` = 闭包的返回值
    /// （`App::update_with` 把它透给调用点，`App::update` 的闭包返回 `()`）。
    pub(crate) fn update_owned<T: 'static, R>(
        &mut self,
        id: SignalId,
        update: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let StateStore {
            values,
            scopes,
            released_scopes,
            ..
        } = self;
        let slot = values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        if *released_scopes > 0
            && scopes
                .get(slot.scope as usize)
                .is_some_and(|scope| scope.released)
        {
            return None;
        }
        Some(slot.update_owned(id, update))
    }

    pub(crate) fn set_transient<T: 'static>(&mut self, id: SignalId, value: T) {
        self.slot_mut(id).write_transient(id, value);
    }

    /// 读普通信号（返回读守卫，不克隆值）。
    pub(crate) fn read_owned<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        self.slot(id).read_owned(id)
    }

    /// 读瞬态值（返回读守卫，不克隆值）。
    pub(crate) fn read_transient<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        self.slot(id).read_transient(id)
    }

    /// 相等比较用的借用视图（不克隆）。`None` = 不是普通信号或类型不符。
    ///
    /// 消费者：`App::write` 的等值短路（借用比较，不克隆）。
    #[cfg(test)]
    pub(crate) fn owned_ref<T: 'static>(&self, id: SignalId) -> Option<&T> {
        self.values.get(id.index() as usize)?.owned_ref::<T>()
    }

    /// 瞬态值的借用视图（不克隆）。`None` = 不是瞬态值或类型不符。
    ///
    /// 消费者：`App::animate` 取动画起点、`ActionCtx` 侧的诊断读。
    pub(crate) fn transient_ref<T: 'static>(&self, id: SignalId) -> Option<&T> {
        self.values.get(id.index() as usize)?.transient_ref::<T>()
    }

    pub(crate) fn mark_derived_dirty(&mut self, id: DerivedId) -> bool {
        self.derived.mark_dirty(id)
    }

    pub(crate) fn derived_handle(&self, id: DerivedId) -> SignalId {
        self.derived.handle(id)
    }

    fn slot(&self, id: SignalId) -> &SignalSlot {
        self.values
            .get(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
    }

    fn slot_mut(&mut self, id: SignalId) -> &mut SignalSlot {
        self.values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
    }

    fn push_slot(&mut self, value: SignalValue, scope: SignalScope) -> SignalId {
        let id = SignalId(u32::try_from(self.values.len()).expect("信号槽位溢出 u32"));
        self.values.push(SignalSlot {
            value,
            scope: scope.index(),
        });
        if let Some(slot) = self.scopes.get_mut(scope.index() as usize) {
            slot.signals.push(id);
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        marker::PhantomData,
        rc::Rc,
    };

    use super::{Derived, Signal, SignalId, SignalRef, StateStore, Transient};
    use crate::{
        app::App,
        error::EngineError,
        node::{builder::NodeBuilder, dependency::ReadCtx, dirty::DirtyFlags, id::NodeId},
        paint::style::StylePatch,
        window::{WindowId, WindowOptions},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime")
    }

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let rt = runtime();
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

    fn deps(app: &App, window: WindowId, node: NodeId) -> Vec<SignalId> {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .deps(node)
            .expect("节点存在")
            .to_vec()
    }

    #[test]
    fn signal_id_is_dense_and_never_reused() {
        let (_rt, mut app) = new_app();
        let a: Signal<u32> = app.signal(1);
        let b: Signal<u32> = app.signal(2);
        let c: Signal<u32> = app.signal(3);

        assert_eq!(
            (a.id().index(), b.id().index(), c.id().index()),
            (0, 1, 2),
            "SignalId 必须是稠密下标（无删除 ⇒ 无自由表 ⇒ 无代际）"
        );
        assert_ne!(a.id(), b.id());
        assert_eq!((a.id().to_bits(), c.id().to_bits()), (0, 2));
    }

    #[test]
    fn signal_handle_is_copy_without_t_bound() {
        struct NotCopy;

        let signal = Signal::<NotCopy> {
            id: SignalId(7),
            _marker: PhantomData,
        };
        let copy = signal;
        let _ = signal;
        assert_eq!(copy.id().index(), 7, "Copy 与 T: Copy 无关");
    }

    #[test]
    fn rewriting_an_owned_value_reuses_its_allocation() {
        // 每帧写同一批信号时不应每帧分配（F-214）。
        let mut store = StateStore::with_capacity(1, 0);
        let id = store.push_owned(String::from("first"));
        let before = store.owned_ref::<String>(id).expect("value") as *const String;
        store.set_owned(id, String::from("second-longer-than-inline"));
        let after = store.owned_ref::<String>(id).expect("value") as *const String;
        assert_eq!(before, after, "同类型写入必须复用已有分配");
        assert_eq!(
            store.owned_ref::<String>(id).expect("value"),
            "second-longer-than-inline"
        );
    }

    #[test]
    fn read_returns_shared_guard_without_copying() {
        // R03-4：读是借用（Rc 递增），不是深拷贝 —— 两次读指向同一个值。
        let mut store = StateStore::with_capacity(1, 0);
        let id = store.push_owned(String::from("price"));

        let first = store.read_owned::<String>(id);
        let second = store.read_owned::<String>(id);

        assert!(
            std::ptr::eq(first.as_ref(), second.as_ref()),
            "读必须共享同一份值（零拷贝）"
        );
        assert_eq!(&*first, "price");
    }

    #[test]
    fn write_equal_value_short_circuits() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(7);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        // R03-9：`refresh_styles` 只消费 `BIND`；`PAINT` 的唯一消费者是 paint pass。
        assert!(!dirty(&app, window, node).contains(DirtyFlags::BIND));

        assert!(!app.write(&signal, 7), "等值写入必须短路");
        assert!(!dirty(&app, window, node).contains(DirtyFlags::BIND));
        assert!(!app.take_wake_request(), "等值写入不得请求唤醒");
    }

    #[test]
    fn write_changed_value_invalidates() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(deps(&app, window, node), vec![signal.id()]);

        assert!(app.write(&signal, 5), "值变化必须返回 true");
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "读者必须被标 PAINT"
        );
        assert!(app.take_wake_request(), "有节点被标脏 → 必须请求唤醒");
    }

    #[test]
    fn equal_but_interior_mutable_value_is_not_repainted() {
        // 等值短路的**风险固化**（R9）：T 内部可变、PartialEq 只比指针 ⇒ 原地修改不会触发重绘。
        let (_rt, mut app) = new_app();
        let shared = Rc::new(Cell::new(1u32));
        let signal: Signal<Rc<Cell<u32>>> = app.signal(Rc::clone(&shared));
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal).get() as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .clear_all_dirty_for_test();
        let _ = app.take_wake_request();

        shared.set(2);
        assert!(
            !app.write(&signal, Rc::clone(&shared)),
            "PartialEq 相等 ⇒ 引擎无法感知内部变化（应用层必须用不可变值语义）"
        );
        assert!(
            !dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "短路意味着**不重绘** —— 这就是 R9 的风险"
        );
        assert!(!app.take_wake_request(), "短路不得请求唤醒");
    }

    #[test]
    fn transient_write_never_invalidates() {
        let (_rt, mut app) = new_app();
        let transient: Transient<f32> = app.transient(0.0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_transient(&transient)),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");

        let before = dirty(&app, window, node);
        app.write_transient(&transient, 1.0);
        assert_eq!(dirty(&app, window, node), before, "transient 写不得标脏");
        assert!(!app.take_wake_request(), "transient 写不得请求唤醒");
    }

    #[test]
    fn transient_read_is_not_tracked() {
        let (_rt, mut app) = new_app();
        let transient: Transient<u32> = app.transient(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_transient(&transient) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert!(
            deps(&app, window, node).is_empty(),
            "transient 读不得登记依赖"
        );
    }

    #[test]
    fn derived_is_lazy() {
        let calls = Rc::new(Cell::new(0u32));
        let counted = Rc::clone(&calls);
        let (_rt, mut app) = new_app();
        let derived: Derived<u32> = app.derived(move |_cx| {
            counted.set(counted.get() + 1);
            42
        });

        assert_eq!(calls.get(), 0, "注册时不得求值");
        assert_eq!(derived.id().index(), 0);
    }

    #[test]
    fn derived_caches_until_invalidated() {
        let calls = Rc::new(Cell::new(0u32));
        let counted = Rc::clone(&calls);
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(1);
        let derived: Derived<u32> = app.derived(move |cx| {
            counted.set(counted.get() + 1);
            *cx.read(&source) * 2
        });
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_derived(&derived).expect("无环") as f32),
            ..StylePatch::default()
        });

        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(calls.get(), 1, "首次读必须求值一次");
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(calls.get(), 1, "未失效时命中缓存");

        assert!(app.write(&source, 3));
        assert!(dirty(&app, window, node).contains(DirtyFlags::PAINT));
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(calls.get(), 2, "失效后必须重算");
    }

    #[test]
    fn derived_chain_propagates_invalidation() {
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(2);
        let inner: Derived<u32> = app.derived(move |cx| *cx.read(&source) + 1);
        let outer: Derived<u32> =
            app.derived(move |cx| *cx.read_derived(&inner).expect("无环") * 10);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_derived(&outer).expect("无环") as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");

        // 节点只依赖 outer 的**句柄**，不直接依赖 source。
        let node_deps = deps(&app, window, node);
        assert_eq!(node_deps.len(), 1);
        assert_ne!(node_deps[0], source.id(), "节点依赖的是派生的句柄 id");

        // 写 source 仍能通过链（source → inner → outer → 节点）失效节点。
        assert!(app.write(&source, 9));
        assert!(dirty(&app, window, node).contains(DirtyFlags::PAINT));
    }

    #[test]
    fn derived_cycle_is_reported_not_panicking() {
        let (_rt, mut app) = new_app();
        let unrelated: Signal<u32> = app.signal(7);
        let slot: Rc<RefCell<Option<Derived<u32>>>> = Rc::new(RefCell::new(None));
        let seen = Rc::new(Cell::new(false));

        let observed = Rc::clone(&seen);
        let alias = Rc::clone(&slot);
        let a: Derived<u32> = app.derived(move |cx| {
            let borrowed = alias.borrow();
            match borrowed.as_ref() {
                Some(derived) => match cx.read_derived(derived) {
                    Ok(value) => *value,
                    Err(EngineError::CyclicDerived { .. }) => {
                        observed.set(true);
                        0
                    }
                    Err(_) => 0,
                },
                None => 0,
            }
        });
        *slot.borrow_mut() = Some(a);

        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_derived(&a).expect("外层不得 panic") as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");

        assert!(seen.get(), "自引必须被 EvalStack 检出并上报 CyclicDerived");
        let node_deps = deps(&app, window, node);
        assert_eq!(node_deps.len(), 1, "节点只依赖派生的**句柄** id");
        assert_ne!(
            node_deps[0],
            unrelated.id(),
            "节点没读过那个普通信号 ⇒ 它不能出现在依赖里（句柄 id 与信号 id 是同一 id 空间）"
        );
        // 环被上报后该派生仍 dirty、缓存为空：写自己的上游不会让节点重绘（没有上游）。
        assert!(!app.take_wake_request());
    }

    #[test]
    fn split_borrow_returns_disjoint_views() {
        let mut store = StateStore::with_capacity(2, 1);
        let id = store.push_owned(1u32);
        let (values, derived) = store.split();

        assert_eq!(values[id.index() as usize].owned_ref::<u32>(), Some(&1));
        assert_eq!(derived.len(), 0);
    }

    // ── R03-4：读守卫（借用，不是拷贝）────────────────────────────────────

    fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
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
    fn read_guard_shares_the_value_instead_of_copying() {
        let (_rt, mut app) = new_app();
        let signal: Signal<String> = app.signal(String::from("BTC"));

        let first = app.read(&signal);
        let second = app.read(&signal);

        assert!(
            std::ptr::eq(first.as_ref(), second.as_ref()),
            "两个读守卫必须指向同一份值（读路径零拷贝）"
        );
        assert_eq!(&*first, "BTC");
        drop((first, second));
        assert!(app.write(&signal, String::from("ETH")), "守卫释放后可写");
    }

    #[test]
    fn write_while_read_guard_is_alive_panics_with_diagnostic() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(1);

        // 守卫**不借用** `App`：因此 `app.write` 能通过借用检查，冲突由运行期显式报告。
        let guard = app.read(&signal);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.write(&signal, 2);
        }))
        .expect_err("读守卫存活期间写同一信号必须 panic");
        let message = panic_payload_message(payload);

        assert!(
            message.contains("SignalRef") || message.contains("read guard"),
            "panic 必须可诊断（指出读守卫借用冲突）：{message}"
        );
        assert!(
            message.contains(&format!("#{}", signal.id().index())),
            "panic 必须带 SignalId：{message}"
        );
        assert_eq!(*guard, 1, "冲突的写不得生效（值保持原样）");
        drop(guard);

        assert!(app.write(&signal, 3), "守卫释放后写必须成功");
        assert_eq!(*app.read(&signal), 3);
    }

    #[test]
    fn read_guard_does_not_block_other_signals() {
        let (_rt, mut app) = new_app();
        let read: Signal<u32> = app.signal(1);
        let written: Signal<u32> = app.signal(10);

        let guard = app.read(&read);
        assert!(app.write(&written, 20), "读 A 不得阻止写 B（粒度是单信号）");
        assert_eq!(*guard, 1);
        assert_eq!(*app.read(&written), 20);
    }

    #[test]
    fn write_after_guard_drop_succeeds_and_invalidates() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        {
            let guard = app.read(&signal);
            assert_eq!(*guard, 0);
        }
        assert!(app.write(&signal, 7));
        assert!(dirty(&app, window, node).contains(DirtyFlags::PAINT));
    }

    #[test]
    fn transient_guard_blocks_transient_write() {
        let (_rt, mut app) = new_app();
        let transient: Transient<f32> = app.transient(0.5);
        let guard = app.read_transient(&transient);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.write_transient(&transient, 1.0);
        }))
        .expect_err("瞬态值的读守卫同样阻止写");
        assert!(panic_payload_message(payload).contains("read guard"));
        assert!((*guard - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn update_mutates_in_place_without_equality_short_circuit() {
        let (_rt, mut app) = new_app();
        let signal: Signal<Vec<u32>> = app.signal(vec![1, 2, 3]);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some((*cx.read(&signal)).len() as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let before = app.read(&signal).as_ref() as *const Vec<u32> as usize;

        assert!(app.update(&signal, |values| values.push(4)));
        let after = app.read(&signal).as_ref() as *const Vec<u32> as usize;
        assert_eq!(before, after, "update 必须原地改（不换分配）");
        assert_eq!(*app.read(&signal), vec![1, 2, 3, 4]);
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "update 无条件失效（无等值短路）"
        );
    }

    #[test]
    fn derived_read_is_a_snapshot_and_does_not_block_writes() {
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(1);
        let derived: Derived<u32> = app.derived(move |cx| *cx.read(&source) * 10);
        // 从 bind 闭包里把派生读守卫**存出来**：守卫不借用 `ReadCtx`（这是借用守卫设计的要点）。
        let captured: Rc<RefCell<Option<SignalRef<u32>>>> = Rc::new(RefCell::new(None));
        let store = Rc::clone(&captured);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            *store.borrow_mut() = Some(cx.read_derived(&derived).expect("无环"));
            StylePatch {
                opacity: Some(*cx.read(&source) as f32),
                ..StylePatch::default()
            }
        });
        app.refresh_styles(window).expect("窗口存在");
        let snapshot = captured.borrow_mut().take().expect("闭包存下了守卫");
        assert_eq!(*snapshot, 10, "派生值 = source × 10（首轮）");

        // 派生读守卫是**缓存快照**：写上游不被阻塞，旧快照保持旧值。
        assert!(app.write(&source, 2), "派生读快照不阻止写上游");
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(
            *snapshot, 10,
            "旧快照保持旧值（快照语义，不失效、不 panic）"
        );
        assert!(
            !dirty(&app, window, node).is_empty(),
            "上游变化沿派生链失效读者"
        );
        drop(snapshot);

        // 重新求值后拿到的是新快照。
        let fresh = {
            app.write(&source, 3);
            app.refresh_styles(window).expect("窗口存在");
            captured.borrow_mut().take().expect("闭包再次存下守卫")
        };
        assert_eq!(*fresh, 30, "新快照反映新上游值");
    }

    // ── A4：批处理（N 次写 ⇒ 1 个帧请求）─────────────────────────────────

    #[test]
    fn batch_coalesces_n_writes_into_one_frame_request() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();

        // 对照组：不批处理 ⇒ 每次写都请求一帧。
        let unbatched_start = app.frame_requests();
        for value in 1..=5 {
            assert!(app.write(&signal, value));
        }
        assert_eq!(
            app.frame_requests() - unbatched_start,
            5,
            "不批处理时 N 次写 ⇒ N 次帧请求"
        );
        let _ = app.take_wake_request();

        // 实验组：批处理 ⇒ 整批只请求一帧。
        let batched_start = app.frame_requests();
        app.batch(|app| {
            for value in 6..=10 {
                assert!(app.write(&signal, value));
            }
            assert!(
                !app.take_wake_request(),
                "批内中间状态不得对外可见（迟迟不置唤醒位）"
            );
        });
        assert_eq!(
            app.frame_requests() - batched_start,
            1,
            "批内 N 次写 ⇒ 1 次帧请求"
        );
        assert!(app.take_wake_request(), "批结束必须请求一帧");
        assert!(!app.take_wake_request(), "且只请求一次");
        assert_eq!(*app.read(&signal), 10, "批内写法照常生效（最终状态可见）");
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::BIND),
            "节点被标 BIND，下一帧样式求值会看到最终值"
        );
    }

    #[test]
    fn batch_does_not_evaluate_derived_values_inline() {
        let calls = Rc::new(Cell::new(0u32));
        let counted = Rc::clone(&calls);
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(1);
        let _derived: Derived<u32> = app.derived(move |cx| {
            counted.set(counted.get() + 1);
            *cx.read(&source) * 2
        });

        app.batch(|app| {
            app.write(&source, 2);
            app.write(&source, 3);
        });
        assert_eq!(calls.get(), 0, "写（含批内）不得重入派生求值");
    }

    #[test]
    fn nested_batches_settle_at_the_outermost_boundary() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let _node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();

        let start = app.frame_requests();
        app.batch(|app| {
            app.write(&signal, 1);
            app.batch(|app| {
                app.write(&signal, 2);
                assert!(app.frame_requests() == start, "内层批结束不得提前请求帧");
            });
            assert!(app.frame_requests() == start, "内层结束后仍在外层批里");
        });
        assert_eq!(app.frame_requests() - start, 1, "最外层结算一次");
    }

    #[test]
    fn batch_panic_unwinds_depth_and_still_requests_the_pending_frame() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let _node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();
        let start = app.frame_requests();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.batch(|app| {
                app.write(&signal, 5);
                panic!("user closure panicked");
            });
        }));
        assert!(result.is_err(), "闭包 panic 原样传播");
        assert!(!app.batching(), "panic 后批处理深度必须归零");
        assert_eq!(
            app.frame_requests() - start,
            1,
            "panic 展开也要把推迟的帧请求结算掉（否则引擎永不唤醒）"
        );
    }

    // ── A2：SignalScope（窗口销毁后不得有僵尸唤醒）──────────────────────────

    #[test]
    fn released_scope_write_is_a_noop_without_frame_request() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal: Signal<u32> = app.window_signal(window, 1);
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert!(!deps(&app, window, node).is_empty());

        assert!(app.close_window(window), "关窗成功");
        let _ = app.take_wake_request();
        let start = app.frame_requests();

        assert!(
            !app.write(&signal, 2),
            "已释放作用域的写必须 short-circuit 为 false"
        );
        assert_eq!(app.frame_requests(), start, "不得请求帧（无僵尸唤醒）");
        assert!(!app.take_wake_request(), "不得置唤醒位");
        assert_eq!(*app.read(&signal), 1, "值被冻结（写没有生效）");
    }

    #[test]
    fn releasing_a_scope_drops_its_dependency_edges_immediately() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        // 用**显式作用域**（不是窗口作用域）：窗口关闭路径会额外逐节点 `detach`，
        // 那条路径无法隔离"release_scope 是否立即删边"。
        let scope = app.signal_scope();
        let signal: Signal<u32> = app.signal_scoped(scope, 1);
        let _node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(
            app.dependency_edge_count_for_test(),
            1,
            "节点对该信号有一条依赖边"
        );

        assert_eq!(app.release_scope(scope), 1);
        assert_eq!(
            app.dependency_edge_count_for_test(),
            0,
            "作用域释放必须**立即**删除依赖边（不是等下次写时惰性清理）"
        );
    }

    // ── A5：句柄用尽的行为 ────────────────────────────────────────────────

    #[test]
    fn window_handle_exhaustion_is_a_diagnostic_panic() {
        let (_rt, mut app) = new_app();
        // 最后一个可用句柄是 `u32::MAX - 1`（铸造后要递增到 `u32::MAX`，再下一个就无值可分配）。
        app.set_next_window_id_for_test(u32::MAX - 1);
        let last = app.create_window(WindowOptions::default());
        assert_eq!(last.to_bits(), u32::MAX - 1, "最后一个可用句柄");

        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.create_window(WindowOptions::default());
        }))
        .expect_err("句柄用尽必须显式失败（不回绕到 0/1）");
        assert!(
            panic_payload_message(payload).contains("exhausted"),
            "panic 必须可诊断"
        );
    }

    #[test]
    fn explicit_scope_release_is_idempotent() {
        let (_rt, mut app) = new_app();
        let scope = app.signal_scope();
        let signal: Signal<u32> = app.signal_scoped(scope, 3);
        assert_eq!(app.release_scope(scope), 1);
        assert_eq!(app.release_scope(scope), 0, "重复释放返回 0");
        assert!(!app.write(&signal, 4));
        assert_eq!(*app.read(&signal), 3);
    }

    // ── A3：NodeId 窗口作用域 ─────────────────────────────────────────────

    #[test]
    fn node_ids_from_different_windows_never_collide() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let second = app.create_window(WindowOptions::default());
        let a = build_node(&mut app, first, |_cx| StylePatch::default());
        let b = build_node(&mut app, second, |_cx| StylePatch::default());

        // 旧实现里两者是**同一个 8 B 句柄**（每窗口 arena 都是槽 1 / 代际 1）。
        assert_eq!(a.index(), b.index(), "两窗都是各自 arena 的第 1 个槽位");
        assert_eq!(a.generation(), b.generation());
        assert_ne!(a, b, "NodeId 必须携带窗口身份");

        let second_tree = app.window(second).expect("窗口存在").tree();
        assert_eq!(
            second_tree.window(),
            Some(second),
            "树必须记录自己的窗口归属"
        );
        assert!(
            matches!(
                second_tree.get(a),
                Err(crate::error::EngineError::StaleNode { node }) if node == a
            ),
            "跨窗口句柄必须**确定性**失败（旧实现会静默命中另一个节点）"
        );
        let first_tree = app.window(first).expect("窗口存在").tree();
        assert!(first_tree.get(a).is_ok());
        assert!(first_tree.get(b).is_err());
    }

    #[test]
    fn cross_window_node_handle_is_rejected_by_app_apis() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let second = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, first, |_cx| StylePatch::default());

        assert!(
            !app.set_focus(second, Some(node)),
            "跨窗口节点不得成为另一窗口的焦点"
        );
        assert!(
            !app.capture_pointer(second, node),
            "跨窗口节点不得成为另一窗口的指针捕获目标"
        );
        assert!(
            !app.remove_node(second, node),
            "跨窗口节点不得被另一窗口卸载"
        );
        assert!(
            app.window(first)
                .expect("窗口存在")
                .tree()
                .get(node)
                .is_ok(),
            "误用不得影响真正的宿主窗口"
        );
    }
}

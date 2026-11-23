//! 状态模型：`Signal`/`Derived`/`Transient` 句柄 + `StateStore`（类型擦除的值存储）。
//!
//! 要点：
//! - 句柄是 `Copy`（`PhantomData<fn() -> T>`，不要求 `T: Copy`），值只存在 `StateStore`；
//! - `SignalId` = **稠密槽位下标 + 代际**（8 B），`DerivedId` 仍是永不删除的稠密下标；
//!   作用域释放会**回收**槽位（值立即 drop、槽位进空闲栈、代际 +1），新信号优先复用空闲
//!   槽位 ⇒ 旧句柄的代际与槽位不匹配，**确定性失效**（见下文〈作用域回收与句柄失效〉）；
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
//! 5. **作用域回收**（[`SignalScope`]）：作用域释放后，其信号的值**立即回收**（槽位归还空闲
//!    栈、代际递增）。此后旧**普通信号**句柄的写是 no-op（返回 `false`、不标脏、不请求帧 ——
//!    迟到写入不得产生僵尸唤醒），读是确定性失败（panic，消息给出槽位/代际/槽位状态）；旧
//!    **瞬态值**句柄的读与写则**都 panic**（`Transient` 的写没有 no-op 形态，见 [`Transient`]）。
//!    无论是哪种，引擎都绝不把复用后的新值交给旧句柄。详见下文〈作用域回收与句柄失效〉。
//!
//! ## 普通信号 / 瞬态值 / 派生值的差别（公开面）
//!
//! | 句柄 | 失效后读 | 失效后写 | 原因 |
//! |---|---|---|---|
//! | [`Signal`] | panic | no-op（`false` / `None`） | 关闭窗口后的迟到写入（异步回投、定时器）不得产生僵尸唤醒（R03-6），也不得把 UI 线程 panic 掉 |
//! | [`Transient`] | panic | **panic** | 写没有「失败」的第二形态（返回 `()`），静默 no-op 会把应用 bug 藏起来；瞬态写与读同口径地显式失败 |
//! | [`Derived`] | panic（重算求值时） | —（无公开写入口） | 派生值不可直接写；上游失效才会重算 |
//!
//! # 作用域回收与句柄失效
//!
//! `App::release_scope` / `App::close_window` 把作用域里的每个值**立即 drop**，槽位下标压回
//! 空闲栈（LIFO、O(1)，不扫描槽位表），槽位代际 +1。之后 `App::signal` /
//! `App::window_signal` / `App::signal_scoped` 优先弹空闲栈复用槽位，铸出的新句柄带**当前**
//! 代际（旧句柄的代际因此必然落后）。
//!
//! 失效判据是一次 `u32` 比较：`SignalId::generation == 槽位代际`。写路径因此**不再需要**旧实现
//! 的 `released_scopes` + 作用域表检查（每个槽位自带判据，热路径没有变慢）。旧句柄的两种行为：
//!
//! | 操作 | 旧句柄（代际不匹配） | 原因 |
//! |---|---|---|
//! | 读（`App::read` / `ReadCtx::read` / `App::read_with` / `ReadCtx::read_transient`） | **panic**（确定性失败），消息含 `stale` + 槽位下标/代际/槽位状态 | 值已被 drop，没有可返回的东西；宁可确定性失败也**不**返回复用后的新值 |
//! | 普通信号写 / 更新（`App::write` / `App::update` / `App::update_with`） | **no-op**（`false` / `None`） | 关闭窗口后的迟到写入（异步回投、定时器）不得产生僵尸唤醒（R03-6），也不得把 UI 线程 panic 掉 |
//! | `App::write_with` | **no-op**（`false`，闭包不会被调用） | 同上：它是「先读旧值再写」组合子，失效时读一步不能 panic |
//! | 瞬态写（`App::write_transient`） | **panic**（与读同口径） | 写返回 `()`，没有「失败」形态；静默 no-op 会把应用 bug 藏起来 |
//!
//! 这个"混合"不是随意的两个政策，而是**唯一的签名约束下的唯一解**：读没有返回值的第二形态
//! （`SignalRef` 不是 `Result`），普通信号的写有既定的 no-op 契约，而瞬态写既无返回值也无
//! no-op 契约。三者的共同保证是**绝不触碰复用后的槽位**——读/瞬态写的 panic 在取值/写入之前，
//! 普通信号的写在同一处代际比较处直接返回。
//!
//! **依赖边是槽位级的**：依赖图的键是完整 `SignalId`（下标 + 代际）。释放时按旧 id 删除全部边；
//! 复用后的新信号代际不同 ⇒ 键不同、不会继承旧边，旧句柄的写也不会遍历任何 owner。
//!
//! **代际用尽**（槽位已发放完 `u32::MAX` 代）⇒ 槽位**永久退休**：代际置 0（不是合法代际，
//! 任何句柄都命中不了）、不进空闲栈。旧句柄因此不会 ABA 复活（与 `node::id::Arena` 同一规则）。

use std::{any::Any, fmt, marker::PhantomData, num::NonZeroU32, ops::Deref, rc::Rc};

use smallvec::SmallVec;

use crate::node::{dependency::ReadCtx, id::next_generation};

/// 信号 / 瞬态值的槽位句柄：**稠密槽位下标 + 代际**（`StateStore::values` 的下标）。
///
/// 8 B（`index` + `generation`；`Option<SignalId>` 同尺寸，`NonZeroU32` 提供 niche）。
///
/// # 代际（槽位回收的必要条件）
///
/// 槽位下标会被**复用**：作用域释放时值被回收、槽位进空闲栈，新信号优先复用。若句柄只有
/// 下标，旧句柄就会静默读到复用后的新值（ABA，R03-8）。代际让失效变成**一次 `u32` 比较**：
/// 句柄的 `generation` 必须等于槽位当前代际，否则该句柄已失效（见模块文档
/// 〈作用域回收与句柄失效〉）。
///
/// `generation` 用 `NonZeroU32` 表示「0 永不是合法代际」：0 只属于**退休槽位**（代际用尽、
/// 不再复用），任何句柄都命中不了 —— 类型即证明。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SignalId {
    index: u32,
    generation: NonZeroU32,
}

impl SignalId {
    /// 槽位下标（诊断/依赖集合用）。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// 代际，恒在 `1..=u32::MAX`（0 不是合法代际，见 [`SignalId`] 的类型不变量）。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// `(generation as u64) << 32 | index as u64`（**槽位局部**编码，与 `NodeId::to_bits` 同布局）。
    ///
    /// 消费者：结构化日志/帧记录需要一个单值可打印的信号标识（设计 §9.1）；同一槽位的历次
    /// 代际在数值上连续递增。
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    /// crate 内构造（`StateStore::push_slot`）。代际不得为 0（0 = 退休槽位）。
    fn from_parts(index: u32, generation: u32) -> Self {
        let Some(generation) = NonZeroU32::new(generation) else {
            panic!("signal generation 0 is reserved for retired slots (engine invariant)");
        };
        Self { index, generation }
    }

    /// 仅测试：直接构造（不进入非 test 构建，因此没有 `dead_code`）。代际恒为 1。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32) -> Self {
        Self {
            index,
            generation: NonZeroU32::MIN,
        }
    }
}

impl fmt::Debug for SignalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SignalId(#{}.{})", self.index, self.generation)
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
        write!(f, "Signal(#{}.{})", self.id.index(), self.id.generation())
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
///
/// **硬禁令（AR5 P2-4 / R5c）：派生闭包不得捕获会随作用域释放的信号句柄。**
///
/// 派生注册在**根作用域**（永不释放），但闭包可以读到任意句柄。若闭包捕获了
/// `App::signal_scoped` / `App::window_signal` 的句柄，而该作用域随后被
/// [`App::release_scope`](crate::app::App::release_scope) 释放，后果是**两个都不会被诊断**的
/// 错误状态：
/// 1. 依赖图里以被释放信号为键的边已被删除 ⇒ 派生**不会再被失效**，缓存永久陈旧；
/// 2. 一旦它因**其它**依赖重算，`ReadCtx::read` 对已失效句柄会**确定性 panic**（UI 线程崩溃）。
///
/// 这类跨作用域捕获不会在释放时被引擎检测（`release_scope` 只清值槽位与边，派生闭包是不透明
/// 的 `dyn Fn`）；因此这是**应用层契约**：派生只能依赖根作用域信号（`App::signal` /
/// `App::transient`），或依赖不随窗口/面板销毁的值。
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
///
/// # 作用域释放后的失效语义
///
/// 用 `App::transient_scoped` 铸造的句柄随所属 [`SignalScope`] 释放而失效：旧句柄的**读与
/// 写都 panic**（确定性失败，绝不读/写槽位复用后的新值）。与普通 [`Signal`] 不同，瞬态写
/// **不是** no-op —— `App::write_transient` 返回 `()`，没有「失败」的第二形态，静默丢弃会把
/// 应用 bug（过期组件仍持有句柄）藏起来。
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
        write!(
            f,
            "Transient(#{}.{})",
            self.id.index(),
            self.id.generation()
        )
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
    pub(crate) fn from_any(value: Rc<dyn Any>, id: SignalId) -> Self {
        match value.downcast::<T>() {
            Ok(value) => Self { value },
            Err(_) => panic!(
                "signal #{}.{}: store value type mismatch — engine invariant broken \
                 (SignalId ↔ T is a constructive bijection)",
                id.index(),
                id.generation()
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
/// app.release_scope(scope);                    // 作用域释放：值回收、旧句柄失效
/// ```
///
/// 释放语义（[`App::release_scope`](crate::app::App::release_scope)）：
/// 1. 作用域内每个信号的**值立即 drop**：槽位下标进空闲栈、槽位代际 +1 —— 内存不再被占着
///    （这是长跑「建窗 → 关窗」churn 时槽位峰值不增长的前提）；
/// 2. 依赖图里以这些信号为键的边被**立即删除**（不是"等下次写时惰性清理"），
///    因此释放后 `dependency_dump` 里查不到它们、`App::write` 也不再遍历任何 owner；
/// 3. **旧句柄失效**（见模块文档〈作用域回收与句柄失效〉）：普通信号句柄的读 **panic**
///    （确定性失败，绝不返回复用后的新值）、写/更新是 **no-op**（`false`/`None`，不标脏、
///    不唤醒）；瞬态值句柄的读与写**都 panic**（见 [`Transient`] 的失效语义）；
/// 4. 已释放的作用域**不能再 mint 新信号**（引擎误用 ⇒ panic）；根作用域（`App::signal` /
///    `App::transient` / `App::derived` 的归属）**永不释放**（`release_scope` 对它返回 0）。
///
/// 槽位复用时旧句柄可能与新信号共享下标，因此**不要**在作用域释放后继续持有/使用句柄：引擎
/// 能做的是让读确定性失败、普通信号写 no-op、瞬态写 panic（见上文），而不是替应用记住"这个
/// 句柄已经死了"。
///
/// **派生闭包同样不得捕获本作用域的句柄（AR5 P2-4 / R5c）**：派生归属根作用域，释放后它既不会
/// 再被失效（依赖边已删 ⇒ 缓存永久陈旧），又会在下次因其它依赖重算时读死句柄 panic。这条禁令
/// 无法在释放时自动检测（派生闭包是不透明的 `dyn Fn`），详见 [`Derived`] 的文档。
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

/// 信号表的一格（**没有**版本号：S-27；**有**代际：回收复用时判定旧句柄失效）。
pub(crate) struct SignalSlot {
    /// 当前占用该槽位的值；`None` = **空闲槽位**（值已随作用域释放被 drop，等待复用）。
    value: Option<SignalValue>,
    /// 槽位代际：存活/空闲槽位恒在 `1..=u32::MAX`；`0` = **退休槽位**（代际用尽，永不复用、
    /// 永不命中任何句柄）。句柄的 [`SignalId::generation`] 必须与它逐位相等才命中。
    generation: u32,
}

impl SignalSlot {
    /// 句柄代际校验：不匹配 ⇒ 句柄已失效（作用域释放后槽位被回收，或已被新信号复用）。
    ///
    /// 读路径用它做**确定性失败**：绝不把复用后的新值交给旧句柄。返回自身，便于链式调用
    /// （`values.get(..).expect(..).live(id).read_owned(id)`）。
    ///
    /// # Panics
    /// 句柄的槽位下标命中，但代际与槽位不同（见模块文档〈作用域回收与句柄失效〉）。
    #[inline]
    pub(crate) fn live(&self, id: SignalId) -> &Self {
        if self.generation != id.generation() {
            stale_handle_panic(id, self);
        }
        self
    }

    /// 空闲槽位的"取值"兜底：走到这里说明代际校验已经放行了，但槽位是空的 ⇒ 引擎不变量破坏。
    #[inline]
    fn expect_value(&self) -> &SignalValue {
        self.value
            .as_ref()
            .expect("live handle resolved to a free signal slot — engine invariant broken")
    }

    /// `SignalValue::Owned` 的借用视图（不克隆、不产生守卫）。
    ///
    /// 消费者：`App::write` 的等值短路（借用比较）。
    #[inline]
    pub(crate) fn owned_ref<T: 'static>(&self) -> Option<&T> {
        match self.value.as_ref()? {
            SignalValue::Owned(value) => value.downcast_ref::<T>(),
            SignalValue::Derived(_) | SignalValue::Transient(_) => None,
        }
    }

    /// `SignalValue::Transient` 的借用视图（不克隆、不产生守卫）。
    pub(crate) fn transient_ref<T: 'static>(&self) -> Option<&T> {
        match self.value.as_ref()? {
            SignalValue::Transient(value) => value.downcast_ref::<T>(),
            SignalValue::Owned(_) | SignalValue::Derived(_) => None,
        }
    }

    /// 读守卫：`Rc` 引用计数递增 + 类型下行（不克隆值）。
    ///
    /// # Panics
    /// 槽位不是普通信号，或其内部类型与 `T` 不一致（引擎不变量被破坏）。
    pub(crate) fn read_owned<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        match self.expect_value() {
            SignalValue::Owned(value) => SignalRef::from_any(Rc::clone(value), id),
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
        match self.expect_value() {
            SignalValue::Transient(value) => SignalRef::from_any(Rc::clone(value), id),
            SignalValue::Owned(_) | SignalValue::Derived(_) => {
                panic!("signal #{}: slot is not a transient value", id.index())
            }
        }
    }

    /// 普通信号的可变值（`write` / `update` 的公共前戏）。
    ///
    /// # Panics
    /// 槽位不是普通信号（瞬态/派生 = 误用；空闲槽位 = 引擎不变量破坏）。
    #[inline]
    fn owned_slot_mut(&mut self, id: SignalId, operation: &'static str) -> &mut Rc<dyn Any> {
        match self.value.as_mut() {
            Some(SignalValue::Owned(existing)) => existing,
            Some(SignalValue::Derived(_) | SignalValue::Transient(_)) => panic!(
                "signal #{}: {operation}() targets a transient/derived slot — use the matching \
                 entry point",
                id.index()
            ),
            None => panic!(
                "signal #{}: {operation}() targets a free slot — engine invariant broken",
                id.index()
            ),
        }
    }

    /// 覆盖普通信号的值：零守卫时**原地**写（复用分配），否则 panic（借用冲突）。
    ///
    /// # Panics
    /// - 存在活跃 [`SignalRef`]：`strong_count > 1`（读守卫持有同一个 `Rc`）；
    /// - 槽位不是普通信号（`write_transient` 误用防护 / 空闲槽位）。
    #[inline]
    pub(crate) fn write_owned<T: 'static>(&mut self, id: SignalId, value: T) {
        let existing = self.owned_slot_mut(id, "write");
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
        let existing = self.owned_slot_mut(id, "update");
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
        let existing = match self.value.as_mut() {
            Some(SignalValue::Transient(existing)) => existing,
            Some(SignalValue::Owned(_) | SignalValue::Derived(_)) => panic!(
                "signal #{}: write_transient() targets a plain/derived slot — use the matching \
                 entry point",
                id.index()
            ),
            None => panic!(
                "signal #{}: write_transient() targets a free slot — engine invariant broken",
                id.index()
            ),
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

/// 旧句柄访问已回收槽位 ⇒ **确定性失败**（见模块文档〈作用域回收与句柄失效〉）。
///
/// 诊断要能定位「句柄来自已释放的作用域 / 槽位现在归谁」，而不是一个裸断言。
#[cold]
fn stale_handle_panic(id: SignalId, slot: &SignalSlot) -> ! {
    let state = match (slot.generation, slot.value.is_some()) {
        (0, _) => "permanently retired (its generation space is exhausted)",
        (_, false) => "free (its value was recycled when the owning scope was released)",
        (_, true) => "occupied by a newer signal (the slot was recycled and reused)",
    };
    panic!(
        "signal #{}.{}: handle is stale — slot #{} is {state}, slot generation {}. \
         A handle must not be used after its scope is released: the value was dropped, so reads \
         fail deterministically instead of returning the new occupant's value. Plain-signal writes \
         are no-ops (`App::write` returns false, `App::update_with` returns None); transient writes \
         (`App::write_transient`) panic like reads.",
        id.index(),
        id.generation(),
        id.index(),
        slot.generation,
    )
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
        let slot = self.slot(id);
        let cache = slot
            .cache
            .as_ref()
            .expect("derived value has not been evaluated");
        SignalRef::from_any(Rc::clone(cache), slot.handle)
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
///
/// 槽位表**只增不减**：`values.len()` 就是槽位峰值（诊断/测试用它证明「窗口 churn 不增长」）。
/// 释放作用域只把槽位下标还给 `free_slots`；真正回收内存的是被 drop 的值。
pub(crate) struct StateStore {
    values: Vec<SignalSlot>,
    /// 空闲槽位栈（LIFO、O(1) 复用；**不扫描**槽位表）。
    ///
    /// 不变量：下标不重复；不含退休槽位（代际用尽时槽位**不进**栈，见
    /// [`StateStore::release_scope`]）；栈上的槽位 `value == None`。
    free_slots: Vec<u32>,
    derived: DerivedStore,
    /// 作用域表；下标 0 = 根作用域（永不释放）。
    scopes: Vec<ScopeSlot>,
}

/// 一个作用域的运行时状态。
struct ScopeSlot {
    /// 是否已释放（释放后不可再 mint 新信号；重复释放是幂等 no-op）。
    released: bool,
    /// 该作用域铸造的、**仍然存活**的信号 id（释放时逐条回收槽位）。
    signals: Vec<SignalId>,
}

impl StateStore {
    pub(crate) fn with_capacity(signals: usize, derived: usize) -> Self {
        Self {
            values: Vec::with_capacity(signals),
            free_slots: Vec::new(),
            derived: DerivedStore::with_capacity(derived),
            scopes: vec![ScopeSlot {
                released: false,
                signals: Vec::new(),
            }],
        }
    }

    /// 拆分借用：信号表只读 + 派生表可变（同一结构体的不交叠字段）。
    pub(crate) fn split(&mut self) -> (&[SignalSlot], &mut DerivedStore) {
        (&self.values, &mut self.derived)
    }

    /// 新建一个作用域（`App::signal_scope` / `App::create_window`）。
    ///
    /// **作用域表本身不回收**：每个建过的窗口留一格 `ScopeSlot`（`bool` + 空 `Vec` 头，32 B；
    /// 值槽位与依赖边都已回收）。回收它需要给 `SignalScope` 也加代际 —— 否则释放后的旧作用域
    /// 句柄会命中并释放**新窗口**的作用域（ABA）；那属于独立的句柄层改动，不在槽位回收范围内。
    pub(crate) fn new_scope(&mut self) -> SignalScope {
        let index = u32::try_from(self.scopes.len()).expect("信号作用域溢出 u32");
        self.scopes.push(ScopeSlot {
            released: false,
            signals: Vec::new(),
        });
        SignalScope::from_index(index)
    }

    /// 某作用域当前**存活**的信号（不含空闲槽位）。
    ///
    /// 已释放 / 根作用域之外越界的作用域返回空切片（根作用域返回其 signals —— 根永不释放，
    /// 调用方需自行区分）。消费者：`App::remove_node_with_scope` 的作用域归属校验
    /// （`window_set::remove`：子树外的存活读者 ⇒ 可证明的错配）。
    pub(crate) fn scope_signals(&self, scope: SignalScope) -> &[SignalId] {
        self.scopes
            .get(scope.index() as usize)
            .filter(|slot| !slot.released)
            .map_or(&[], |slot| slot.signals.as_slice())
    }

    /// 释放作用域：**回收**它的全部值槽位（值立即 drop、槽位归还空闲栈、代际 +1），返回旧句柄
    /// （调用方负责按旧 id 删除依赖图里的边）。
    ///
    /// 重复释放返回空 `Vec`（幂等）。根作用域不可释放（`None`）。
    ///
    /// 代际用尽（槽位已发放完 `u32::MAX` 代）的槽位**永久退休**：代际置 0（非法代际）、不进
    /// 空闲栈 —— 旧句柄因此永远不会 ABA 复活（与 `node::id::Arena` 同一规则）。
    pub(crate) fn release_scope(&mut self, scope: SignalScope) -> Option<Vec<SignalId>> {
        // 根作用域承载 `App::signal` / `transient` / `derived`：永不释放。旧实现只在文档里承诺
        // 这一点、实现却会照常"释放"；槽位回收后那会清空整个应用的状态，必须做成硬约束。
        if scope.index() == SignalScope::ROOT.index() {
            return None;
        }
        let scope_slot = self.scopes.get_mut(scope.index() as usize)?;
        if scope_slot.released {
            return Some(Vec::new());
        }
        scope_slot.released = true;
        let signals = std::mem::take(&mut scope_slot.signals);
        for id in &signals {
            let slot = self
                .values
                .get_mut(id.index() as usize)
                .expect("released signal id out of range: engine invariant broken");
            debug_assert_eq!(
                slot.generation,
                id.generation(),
                "released id must be the live occupant of its slot"
            );
            // 值立即 drop（`Rc` 归零 ⇒ T 的内存还给分配器）。在外的读守卫持有自己的 `Rc`
            // 快照，因此不受影响（见模块文档的借用规则）。
            slot.value = None;
            match next_generation(slot.generation) {
                Some(next) => {
                    slot.generation = next;
                    self.free_slots.push(id.index());
                }
                None => slot.generation = 0,
            }
        }
        Some(signals)
    }

    pub(crate) fn push_owned<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_owned_scoped(SignalScope::ROOT, value)
    }

    /// 在指定作用域里注册普通信号。
    ///
    /// # Panics
    /// 作用域已释放（引擎误用：释放后不可再 mint 新信号）。
    pub(crate) fn push_owned_scoped<T: 'static>(
        &mut self,
        scope: SignalScope,
        value: T,
    ) -> SignalId {
        self.push_slot(SignalValue::Owned(Rc::new(value)), scope)
    }

    pub(crate) fn push_transient<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_transient_scoped(SignalScope::ROOT, value)
    }

    /// 在指定作用域里注册瞬态值（随作用域释放回收）。
    ///
    /// # Panics
    /// 作用域已释放（引擎误用：释放后不可再 mint 值）。
    pub(crate) fn push_transient_scoped<T: 'static>(
        &mut self,
        scope: SignalScope,
        value: T,
    ) -> SignalId {
        self.push_slot(SignalValue::Transient(Rc::new(value)), scope)
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
    /// 槽位不是普通信号、句柄已失效，或该信号存在活跃读守卫（见 [`SignalSlot::write_owned`]）。
    #[cfg(test)]
    pub(crate) fn set_owned<T: 'static>(&mut self, id: SignalId, value: T) {
        self.live_slot_mut(id).write_owned(id, value);
    }

    /// **写路径的合并入口**（一次槽位查找）：句柄代际校验 → 等值短路 → 借用冲突检查 → 写。
    ///
    /// 返回值：`None` = **句柄已失效**（作用域已释放 / 槽位已复用：no-op，调用方不失效、
    /// 不唤醒 —— 迟到写入不得产生僵尸唤醒，也绝不写复用后的新值）；`Some(false)` = 等值短路；
    /// `Some(true)` = 已写入（调用方负责 `invalidate`）。
    ///
    /// 为什么合并：`App::write` 是最高频热路径（行情推送每帧成百上千次）；代际比较是一条
    /// `u32` 比较（它取代了旧实现的 `released_scopes` + 作用域表 Option 链，热路径没有变慢）。
    #[inline]
    pub(crate) fn write_owned<T: PartialEq + 'static>(
        &mut self,
        id: SignalId,
        value: T,
    ) -> Option<bool> {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        if slot.generation != id.generation() {
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
    /// 返回值：`None` = 句柄已失效（no-op，闭包不会被调用）；`Some(r)` = 闭包的返回值
    /// （`App::update_with` 把它透给调用点，`App::update` 的闭包返回 `()`）。
    pub(crate) fn update_owned<T: 'static, R>(
        &mut self,
        id: SignalId,
        update: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        if slot.generation != id.generation() {
            return None;
        }
        Some(slot.update_owned(id, update))
    }

    pub(crate) fn set_transient<T: 'static>(&mut self, id: SignalId, value: T) {
        self.live_slot_mut(id).write_transient(id, value);
    }

    /// 读普通信号（返回读守卫，不克隆值）。
    ///
    /// # Panics
    /// 句柄已失效（**确定性失败**，见模块文档〈作用域回收与句柄失效〉），或槽位不是普通信号。
    pub(crate) fn read_owned<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        self.live_slot(id).read_owned(id)
    }

    /// 读瞬态值（返回读守卫，不克隆值）。
    ///
    /// # Panics
    /// 句柄已失效，或槽位不是瞬态值。
    pub(crate) fn read_transient<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        self.live_slot(id).read_transient(id)
    }

    /// 相等比较用的借用视图（不克隆）。`None` = 句柄已失效 / 不是普通信号 / 类型不符。
    ///
    /// 消费者：`App::write` 的等值短路（借用比较，不克隆）。
    #[cfg(test)]
    pub(crate) fn owned_ref<T: 'static>(&self, id: SignalId) -> Option<&T> {
        self.slot_if_live(id)?.owned_ref::<T>()
    }

    /// 瞬态值的借用视图（不克隆）。`None` = 句柄已失效 / 不是瞬态值 / 类型不符。
    ///
    /// 消费者：`App::animate` 取动画起点、`ActionCtx` 侧的诊断读。
    pub(crate) fn transient_ref<T: 'static>(&self, id: SignalId) -> Option<&T> {
        self.slot_if_live(id)?.transient_ref::<T>()
    }

    /// 句柄是否仍指向存活槽位。
    ///
    /// 消费者：`App::write_with` —— 它是「先读旧值再写」组合子，失效句柄必须走 no-op 而不是
    /// 在读一步 panic（与 `App::write` 同口径）。
    #[inline]
    pub(crate) fn is_live(&self, id: SignalId) -> bool {
        self.slot_if_live(id).is_some()
    }

    pub(crate) fn mark_derived_dirty(&mut self, id: DerivedId) -> bool {
        self.derived.mark_dirty(id)
    }

    pub(crate) fn derived_handle(&self, id: DerivedId) -> SignalId {
        self.derived.handle(id)
    }

    /// 槽位表长度 = **槽位峰值**（数组只增不减：回收只把下标放进空闲栈）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn slot_count_for_test(&self) -> usize {
        self.values.len()
    }

    /// 空闲栈长度（已回收、等待复用的槽位数）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn free_slot_count_for_test(&self) -> usize {
        self.free_slots.len()
    }

    /// 存活（仍持有值）槽位数。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn live_slot_count_for_test(&self) -> usize {
        self.values
            .iter()
            .filter(|slot| slot.value.is_some())
            .count()
    }

    /// 仅测试：把某个存活槽位的代际推到指定值，返回与新代际匹配的句柄（验证退休路径）。
    ///
    /// 同时改写作用域表里登记的 id —— 它必须与槽位保持一致，否则释放时会撞上 debug 断言。
    #[cfg(test)]
    pub(crate) fn force_generation_for_test(&mut self, old: SignalId, generation: u32) -> SignalId {
        let slot = self
            .values
            .get_mut(old.index() as usize)
            .expect("test handle must be alive");
        slot.generation = generation;
        let fresh = SignalId::from_parts(old.index(), generation);
        for scope in &mut self.scopes {
            for id in &mut scope.signals {
                if *id == old {
                    *id = fresh;
                }
            }
        }
        fresh
    }

    /// 句柄命中且代际一致时的只读槽位（失效 ⇒ `None`）。
    fn slot_if_live(&self, id: SignalId) -> Option<&SignalSlot> {
        let slot = self.values.get(id.index() as usize)?;
        (slot.generation == id.generation()).then_some(slot)
    }

    /// 读路径的槽位查找：句柄失效 ⇒ 确定性 panic（见 [`SignalSlot::live`]）。
    fn live_slot(&self, id: SignalId) -> &SignalSlot {
        self.values
            .get(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
            .live(id)
    }

    /// 冷路径的槽位查找（`write_transient` / 测试写）：句柄失效 ⇒ 确定性 panic。
    fn live_slot_mut(&mut self, id: SignalId) -> &mut SignalSlot {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        if slot.generation != id.generation() {
            stale_handle_panic(id, slot);
        }
        slot
    }

    /// 铸出新句柄并登记进作用域：优先复空闲栈里的槽位（代际已在释放时 +1），否则追加新槽位。
    ///
    /// # Panics
    /// 作用域已释放，或槽位下标溢出 `u32`。
    fn push_slot(&mut self, value: SignalValue, scope: SignalScope) -> SignalId {
        let scope_slot = self
            .scopes
            .get_mut(scope.index() as usize)
            .unwrap_or_else(|| {
                panic!(
                    "signal scope #{} does not exist — engine invariant broken",
                    scope.index()
                )
            });
        if scope_slot.released {
            panic!(
                "signal scope #{}: released scopes cannot mint new signals — its values were \
                 recycled by `release_scope`, so a new value here would leak forever",
                scope.index()
            );
        }
        let (index, generation) = match self.free_slots.pop() {
            Some(index) => {
                let generation = self.values[index as usize].generation;
                debug_assert!(generation > 0, "free list must never hold a retired slot");
                (index, generation)
            }
            None => {
                let index = u32::try_from(self.values.len()).expect("信号槽位溢出 u32");
                self.values.push(SignalSlot {
                    value: None,
                    generation: 1,
                });
                (index, 1)
            }
        };
        self.values[index as usize].value = Some(value);
        let id = SignalId::from_parts(index, generation);
        scope_slot.signals.push(id);
        id
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        marker::PhantomData,
        mem::size_of,
        rc::Rc,
    };

    use super::{Derived, Signal, SignalId, SignalRef, SignalScope, StateStore, Transient};
    use crate::{
        app::App,
        error::EngineError,
        node::{
            builder::NodeBuilder,
            dependency::{DependencyGraph, Owner, ReadCtx},
            dirty::DirtyFlags,
            id::NodeId,
        },
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
    fn signal_id_is_a_dense_slot_index_plus_generation() {
        let (_rt, mut app) = new_app();
        let a: Signal<u32> = app.signal(1);
        let b: Signal<u32> = app.signal(2);
        let c: Signal<u32> = app.signal(3);

        assert_eq!(
            (a.id().index(), b.id().index(), c.id().index()),
            (0, 1, 2),
            "SignalId 是稠密槽位下标（根作用域不回收 ⇒ 只增）"
        );
        assert_eq!(
            (
                a.id().generation(),
                b.id().generation(),
                c.id().generation()
            ),
            (1, 1, 1),
            "新槽位的代际从 1 开始（0 只属于退休槽位）"
        );
        assert_ne!(a.id(), b.id());
        assert_eq!(
            a.id().to_bits(),
            1u64 << 32,
            "to_bits = (代际 << 32) | 下标"
        );
        assert_eq!(c.id().to_bits(), (1u64 << 32) | 2);
        assert_eq!(size_of::<SignalId>(), 8, "槽位下标 + 代际");
        assert_eq!(
            size_of::<Option<SignalId>>(),
            8,
            "NonZeroU32 的 niche 让 Option 不额外膨胀"
        );
    }

    #[test]
    fn signal_handle_is_copy_without_t_bound() {
        struct NotCopy;

        let signal = Signal::<NotCopy> {
            id: SignalId::for_test(7),
            _marker: PhantomData,
        };
        let copy = signal;
        let _ = signal;
        assert_eq!(copy.id().index(), 7, "Copy 与 T: Copy 无关");
        assert_eq!(copy.id().generation(), 1);
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

    // ── A2：SignalScope（窗口销毁后不得有僵尸唤醒；S6 起值也立即回收）──────────

    #[test]
    fn released_scope_handle_write_is_a_noop_and_read_fails_deterministically() {
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

        // 写 / 更新：no-op（false / None、不标脏、不唤醒）—— 迟到写入不得僵尸唤醒（R03-6）。
        assert!(
            !app.write(&signal, 2),
            "已失效句柄的写必须 short-circuit 为 false"
        );
        assert!(
            app.update_with(&signal, |value| *value += 1).is_none(),
            "已失效句柄的 update_with 必须返回 None"
        );
        assert_eq!(app.frame_requests(), start, "不得请求帧（无僵尸唤醒）");
        assert!(!app.take_wake_request(), "不得置唤醒位");

        // 读：确定性失败（值已被回收，绝不能返回复用后的新值）。
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = *app.read(&signal);
        }))
        .expect_err("读已失效句柄必须 panic");
        let message = panic_payload_message(payload);
        assert!(
            message.contains("stale"),
            "panic 必须可诊断（stale）：{message}"
        );
        assert!(
            message.contains(&format!(
                "#{}.{}",
                signal.id().index(),
                signal.id().generation()
            )),
            "panic 必须带槽位下标与代际：{message}"
        );
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

    // ── S6：槽位回收 + 复用 + 句柄失效（`StateStore` 层）────────────────────
    //
    // 窗口路径（`create_window` / `window_signal` / `close_window`）对应的 store 调用就是
    // `new_scope` / `push_owned_scoped` / `release_scope` + 逐 id `graph.remove_signal`，
    // 因此这一层的断言直接覆盖「建窗 → 加信号 → 关窗」循环的存储行为。

    /// 回收后复用同一槽位、代际递增；旧句柄不等于新句柄。
    #[test]
    fn releasing_a_scope_recycles_slots_and_bumps_generation() {
        let mut store = StateStore::with_capacity(4, 0);
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 7u32);
        assert_eq!((old.index(), old.generation()), (0, 1));
        assert_eq!(store.live_slot_count_for_test(), 1);

        assert_eq!(store.release_scope(scope).map(|ids| ids.len()), Some(1));
        assert_eq!(store.slot_count_for_test(), 1, "槽位数组不收缩（下标复用）");
        assert_eq!(store.free_slot_count_for_test(), 1, "槽位必须归还空闲栈");
        assert_eq!(
            store.live_slot_count_for_test(),
            0,
            "值必须被 drop（内存回收）"
        );
        assert!(!store.is_live(old), "释放后旧句柄立即失效");

        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 100u32);
        assert_eq!(fresh.index(), old.index(), "新信号必须优先复用空闲槽位");
        assert_eq!(
            fresh.generation(),
            old.generation() + 1,
            "复用必须推进槽位代际"
        );
        assert_ne!(fresh, old);
        assert_eq!(store.free_slot_count_for_test(), 0);
        assert_eq!(store.live_slot_count_for_test(), 1);
        assert!(store.is_live(fresh));
        assert_eq!(*store.read_owned::<u32>(fresh), 100);
    }

    /// 读旧句柄 = 确定性失败（panic 带诊断），且永远读不到复用后的新值。
    #[test]
    fn stale_handle_read_is_a_deterministic_panic_with_diagnostic() {
        let mut store = StateStore::with_capacity(2, 0);
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 1u32);
        store.release_scope(scope);
        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_eq!(fresh.index(), old.index());

        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = *store.read_owned::<u32>(old);
        }))
        .expect_err("旧句柄读必须 panic（绝不返回复用后的新值）");
        let message = panic_payload_message(payload);
        assert!(message.contains("stale"), "panic 必须可诊断：{message}");
        assert!(
            message.contains("newer signal") || message.contains("free"),
            "panic 必须说明槽位现状（空闲 / 已被新信号占用）：{message}"
        );
        assert!(
            message.contains("#0.1"),
            "panic 必须带旧句柄下标+代际：{message}"
        );
        assert_eq!(*store.read_owned::<u32>(fresh), 2, "新占用者的值不受影响");
    }

    /// 写旧句柄 = no-op（`None`），绝不修改复用后的新值。
    #[test]
    fn stale_handle_write_is_a_noop_and_never_touches_the_reused_value() {
        let mut store = StateStore::with_capacity(2, 0);
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 1u32);
        store.release_scope(scope);
        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_eq!(fresh.index(), old.index());

        assert_eq!(
            store.write_owned(old, 99u32),
            None,
            "失效句柄的写必须是 no-op（None = 不失效传播）"
        );
        assert_eq!(
            store.update_owned(old, |value| *value = 100),
            None,
            "失效句柄的 update 必须返回 None（闭包不执行）"
        );
        assert_eq!(
            *store.read_owned::<u32>(fresh),
            2,
            "复用后的新值绝不能被子虚乌有的旧写改掉"
        );
        assert_eq!(store.write_owned(fresh, 3u32), Some(true), "新句柄照常可写");
        assert_eq!(*store.read_owned::<u32>(fresh), 3);
    }

    /// 释放前已发出的读守卫是 `Rc` 快照：值被回收后它仍可读（快照语义，不悬垂）。
    #[test]
    fn read_guard_survives_the_release_as_a_value_snapshot() {
        let mut store = StateStore::with_capacity(1, 0);
        let scope = store.new_scope();
        let id = store.push_owned_scoped(scope, String::from("frozen"));

        let guard = store.read_owned::<String>(id);
        store.release_scope(scope);
        assert_eq!(&*guard, "frozen", "已发出的守卫持有旧值的 Rc 快照");
        assert_eq!(store.live_slot_count_for_test(), 0, "存储侧的值已 drop");
    }

    /// 代际用尽 ⇒ 槽位永久退休：不进空闲栈、不被复用，旧句柄不会 ABA 复活。
    #[test]
    fn generation_exhaustion_retires_the_slot_instead_of_wrapping() {
        let mut store = StateStore::with_capacity(1, 0);
        let scope = store.new_scope();
        let id = store.push_owned_scoped(scope, 1u32);
        let almost = store.force_generation_for_test(id, u32::MAX);

        assert_eq!(store.release_scope(scope).map(|ids| ids.len()), Some(1));
        assert!(!store.is_live(almost));
        assert_eq!(store.free_slot_count_for_test(), 0, "退休槽位不进空闲栈");

        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_ne!(fresh.index(), id.index(), "退休槽位不得被复用");
        assert_eq!(store.slot_count_for_test(), 2, "新信号只能用新槽位");
        assert!(!store.is_live(almost), "旧句柄不得复活");
    }

    /// 1000 轮「建窗 → 加信号 → 关窗」：槽位峰值**不增长**（去掉回收 ⇒ 线性增长 ⇒ 红）。
    #[test]
    fn thousand_window_cycles_recycle_slots_so_the_peak_does_not_grow() {
        const CYCLES: usize = 1000;
        const SIGNALS_PER_WINDOW: usize = 8;

        let mut store = StateStore::with_capacity(4, 0);
        let mut peak: Option<usize> = None;
        for cycle in 0..CYCLES {
            let scope = store.new_scope();
            for value in 0..SIGNALS_PER_WINDOW {
                let id = store.push_owned_scoped(scope, (value + cycle) as u32);
                assert!(store.is_live(id), "新句柄必须存活");
            }
            assert_eq!(
                store.release_scope(scope).map(|ids| ids.len()),
                Some(SIGNALS_PER_WINDOW)
            );
            assert_eq!(store.live_slot_count_for_test(), 0, "关窗后不得有存活槽位");
            let slots = store.slot_count_for_test();
            match peak {
                Some(expected) => assert_eq!(
                    slots, expected,
                    "第 {cycle} 轮后槽位峰值必须停在第 1 轮的 {expected} 个槽位（回收失效会线性增长）"
                ),
                None => peak = Some(slots),
            }
        }
        assert_eq!(
            store.free_slot_count_for_test(),
            SIGNALS_PER_WINDOW,
            "最后一轮的槽位必须全部回到空闲栈"
        );
    }

    /// 槽位级依赖边：释放后按旧 id 归零；复用后的新信号不继承旧 owner。
    #[test]
    fn recycling_a_slot_clears_its_dependency_entries() {
        let mut store = StateStore::with_capacity(2, 0);
        let mut graph = DependencyGraph::new();
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 1u32);
        let owner = Owner::Node {
            window: WindowId::for_test(1),
            node: NodeId::for_test(1, 1),
        };
        graph.replace_deps(owner, &[], &[old]);
        assert_eq!(graph.signal_count(), 1, "先建立一条依赖边");

        // 与 `App::release_scope` 完全同一条调用点：释放槽位 + 逐 id 删边。
        let released = store.release_scope(scope).expect("作用域存在");
        for signal in &released {
            graph.remove_signal(*signal);
        }
        assert_eq!(graph.signal_count(), 0, "释放后槽位级依赖边必须归零");

        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_eq!(fresh.index(), old.index(), "槽位已复用");
        assert!(
            graph.dependents_mut(fresh).is_none(),
            "新占用者不得继承旧信号的 owner"
        );
        assert!(graph.dependents_mut(old).is_none(), "旧键必须干净");
    }

    /// 已释放作用域不得再 mint 信号（否则新值永远没有释放入口 = 重新引入泄漏）。
    #[test]
    fn minting_into_a_released_scope_is_a_diagnostic_panic() {
        let mut store = StateStore::with_capacity(1, 0);
        let scope = store.new_scope();
        store.push_owned_scoped(scope, 1u32);
        store.release_scope(scope);

        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.push_owned_scoped(scope, 2u32);
        }))
        .expect_err("已释放作用域不得再 mint 信号");
        let message = panic_payload_message(payload);
        assert!(
            message.contains("released scopes cannot mint"),
            "panic 必须可诊断：{message}"
        );
    }

    /// 根作用域没有释放入口（否则会把 `App::signal` / transient / derived 全部回收）。
    #[test]
    fn root_scope_is_never_released() {
        let mut store = StateStore::with_capacity(2, 0);
        let root_signal = store.push_owned(1u32);
        assert_eq!(
            store.release_scope(SignalScope::ROOT),
            None,
            "根作用域不可释放"
        );
        assert!(store.is_live(root_signal), "根作用域信号必须仍然存活");
        assert_eq!(*store.read_owned::<u32>(root_signal), 1);
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
        assert!(!app.write(&signal, 4), "失效句柄的写是 no-op");
        // 值已回收：读是确定性失败（不再是"返回冻结值"）。
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = *app.read(&signal);
        }))
        .expect_err("失效句柄的读必须 panic");
        assert!(panic_payload_message(payload).contains("stale"));
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
            matches!(
                app.remove_node(second, node),
                Err(EngineError::StaleNode { node: reported }) if reported == node
            ),
            "跨窗口节点不得被另一窗口卸载（强类型 StaleNode）"
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

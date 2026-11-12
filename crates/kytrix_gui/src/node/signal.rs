//! 状态模型：`Signal`/`Derived`/`Transient` 句柄 + `StateStore`（类型擦除的值存储）。
//!
//! 设计见 `design/phase-03-node-state.md` §7。要点：
//! - 句柄是 `Copy`（`PhantomData<fn() -> T>`，不要求 `T: Copy`），值只存在 `StateStore`；
//! - `SignalId`/`DerivedId` 是**稠密索引**，条目永不删除 ⇒ 无代际、无自由表；
//! - 派生值**惰性**求值、push 失效；环检测在运行时求值栈上（见 `node::dependency::EvalStack`）；
//! - **没有版本号**：失效是 push 模型，等值短路由 `App::write` 的 `PartialEq` 完成（S-27 / S-35）。

use std::{any::Any, marker::PhantomData};

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

/// 信号表的一格：普通信号 / 派生句柄 / 瞬态值。
pub(crate) enum SignalValue {
    /// 普通信号：类型擦除的值。
    Owned(Box<dyn Any>),
    /// 派生值的**句柄槽**：值在 `DerivedSlot::cache`。
    Derived(DerivedId),
    /// 瞬态值：类型擦除的值；读不登记、写不失效。
    Transient(Box<dyn Any>),
}

/// 信号表的一格（**没有**版本号：S-27）。
pub(crate) struct SignalSlot {
    value: SignalValue,
}

impl SignalSlot {
    /// `SignalValue::Owned` 的 downcast + clone。
    ///
    /// # Panics
    /// 槽位类型不是 `Owned`，或其内部类型与 `T` 不一致（引擎不变量被破坏：`SignalId ↔ 类型` 是构造性双射）。
    pub(crate) fn owned<T: Clone + 'static>(&self) -> T {
        match &self.value {
            SignalValue::Owned(value) => value
                .downcast_ref::<T>()
                .expect("signal value type mismatch: engine invariant broken")
                .clone(),
            SignalValue::Derived(derived) => {
                panic!("signal slot is the handle of derived {derived:?}, not a plain signal")
            }
            SignalValue::Transient(_) => panic!("signal slot is not a plain signal"),
        }
    }

    /// `SignalValue::Transient` 的 downcast + clone。
    ///
    /// # Panics
    /// 槽位类型不是 `Transient`，或其内部类型与 `T` 不一致。
    pub(crate) fn transient<T: Clone + 'static>(&self) -> T {
        match &self.value {
            SignalValue::Transient(value) => value
                .downcast_ref::<T>()
                .expect("transient value type mismatch: engine invariant broken")
                .clone(),
            SignalValue::Owned(_) | SignalValue::Derived(_) => {
                panic!("signal slot is not a transient value")
            }
        }
    }
}

/// 派生槽：闭包 + 缓存 + 脏位 + 上次依赖。
pub(crate) struct DerivedSlot {
    /// 该派生值在信号表里的句柄槽 id（依赖图只用 `SignalId` 作键）。
    handle: SignalId,
    /// 计算闭包。求值期 `take()` 出去（避免与 `&mut self` 别名），求值后放回。
    compute: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>>,
    /// 缓存值（`Some` 且 `dirty == false` 时有效）。
    cache: Option<Box<dyn Any>>,
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
    pub(crate) fn store_value(&mut self, id: DerivedId, value: Box<dyn Any>) {
        let slot = self.slot_mut(id);
        slot.cache = Some(value);
        slot.dirty = false;
    }

    /// 读缓存（downcast + clone）。
    ///
    /// # Panics
    /// 尚未求值（调用方保证先检查 `is_dirty`），或类型不一致（不变量破坏）。
    pub(crate) fn value<T: Clone + 'static>(&self, id: DerivedId) -> T {
        self.slot(id)
            .cache
            .as_ref()
            .expect("derived value has not been evaluated")
            .downcast_ref::<T>()
            .expect("derived value type mismatch: engine invariant broken")
            .clone()
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

/// 信号 / 派生 / 瞬态值的唯一存储。
pub(crate) struct StateStore {
    values: Vec<SignalSlot>,
    derived: DerivedStore,
}

impl StateStore {
    pub(crate) fn with_capacity(signals: usize, derived: usize) -> Self {
        Self {
            values: Vec::with_capacity(signals),
            derived: DerivedStore::with_capacity(derived),
        }
    }

    /// 拆分借用：信号表只读 + 派生表可变（同一结构体的不交叠字段）。
    pub(crate) fn split(&mut self) -> (&[SignalSlot], &mut DerivedStore) {
        (&self.values, &mut self.derived)
    }

    pub(crate) fn push_owned<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_slot(SignalValue::Owned(Box::new(value)))
    }

    pub(crate) fn push_transient<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_slot(SignalValue::Transient(Box::new(value)))
    }

    pub(crate) fn push_derived(
        &mut self,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) -> (DerivedId, SignalId) {
        let derived_id = DerivedId(u32::try_from(self.derived.len()).expect("派生槽位溢出 u32"));
        let handle = self.push_slot(SignalValue::Derived(derived_id));
        self.derived.push(handle, compute);
        (derived_id, handle)
    }

    /// 覆盖普通信号的值（**不**比较、**不**失效：失效由 `App::write` 负责）。
    ///
    /// # Panics (debug)
    /// 槽位不是普通信号（`write_transient` 误用防护）。
    pub(crate) fn set_owned<T: 'static>(&mut self, id: SignalId, value: T) {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        debug_assert!(
            matches!(slot.value, SignalValue::Owned(_)),
            "transient/derived 必须用对应的写入口"
        );
        slot.value = SignalValue::Owned(Box::new(value));
    }

    pub(crate) fn set_transient<T: 'static>(&mut self, id: SignalId, value: T) {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        debug_assert!(
            matches!(slot.value, SignalValue::Transient(_)),
            "普通信号必须用 write"
        );
        slot.value = SignalValue::Transient(Box::new(value));
    }

    /// 相等比较用的借用视图（不克隆）。`None` = 不是普通信号或类型不符。
    ///
    /// 消费者：`App::write` 的等值短路（借用比较，不克隆）。
    pub(crate) fn owned_ref<T: 'static>(&self, id: SignalId) -> Option<&T> {
        match &self.values.get(id.index() as usize)?.value {
            SignalValue::Owned(value) => value.downcast_ref::<T>(),
            SignalValue::Derived(_) | SignalValue::Transient(_) => None,
        }
    }

    pub(crate) fn mark_derived_dirty(&mut self, id: DerivedId) -> bool {
        self.derived.mark_dirty(id)
    }

    pub(crate) fn derived_handle(&self, id: DerivedId) -> SignalId {
        self.derived.handle(id)
    }

    fn push_slot(&mut self, value: SignalValue) -> SignalId {
        let id = SignalId(u32::try_from(self.values.len()).expect("信号槽位溢出 u32"));
        self.values.push(SignalSlot { value });
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

    use super::{Derived, Signal, SignalId, SignalSlot, SignalValue, StateStore, Transient};
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
    fn read_clones_value() {
        let slot = SignalSlot {
            value: SignalValue::Owned(Box::new(String::from("price"))),
        };

        let mut first = slot.owned::<String>();
        first.push('!');
        let second = slot.owned::<String>();

        assert_eq!(second, "price", "读必须返回克隆，不得影响存储");
    }

    #[test]
    fn write_equal_value_short_circuits() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(7);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert!(!dirty(&app, window, node).contains(DirtyFlags::PAINT));

        assert!(!app.write(&signal, 7), "等值写入必须短路");
        assert!(!dirty(&app, window, node).contains(DirtyFlags::PAINT));
        assert!(!app.take_wake_request(), "等值写入不得请求唤醒");
    }

    #[test]
    fn write_changed_value_invalidates() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
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
            opacity: Some(cx.read_transient(&transient)),
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
            opacity: Some(cx.read_transient(&transient) as f32),
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
            cx.read(&source) * 2
        });
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read_derived(&derived).expect("无环") as f32),
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
        let inner: Derived<u32> = app.derived(move |cx| cx.read(&source) + 1);
        let outer: Derived<u32> =
            app.derived(move |cx| cx.read_derived(&inner).expect("无环") * 10);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read_derived(&outer).expect("无环") as f32),
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
                    Ok(value) => value,
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
            opacity: Some(cx.read_derived(&a).expect("外层不得 panic") as f32),
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

        assert_eq!(values[id.index() as usize].owned::<u32>(), 1);
        assert_eq!(derived.len(), 0);
    }
}

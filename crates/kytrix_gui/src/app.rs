//! `App`：状态表 + 依赖图 + 窗口集合 + runtime 句柄。
//!
//! 设计见 `design/phase-03-node-state.md` §5.1 / §14。P03 是 headless 的：没有事件循环、没有
//! `Application`/`AppHandle`（推迟 P08/P09），`App` 只负责「状态 + 窗口 + 语义脏标记」。
//!
//! 写路径是 push 模型（§7.5）：一次 `write` 沿依赖图把 owner 标脏，**不**遍历整棵树；
//! 帧内唯一的重活是 `refresh_styles`（样式求值 pass，P06 的 paint 阶段复用同一实现）。

use std::any::Any;

use smallvec::{SmallVec, smallvec};

use crate::{
    node::{
        dependency::{DependencyGraph, EvalStack, Owner, ReadCtx, ReadTracker},
        dirty::DirtyFlags,
        signal::{Derived, Signal, SignalId, StateStore, Transient},
    },
    paint::style::StylePatch,
    window::{Window, WindowId, WindowOptions},
};

/// 一次样式求值 pass 的结果（P06 的 `FrameRecord` 会复用这些字段）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StylePassReport {
    /// 实际求值的 `bind_style` 闭包个数（不区分「补丁是否变化」）。
    pub evaluated: usize,
    /// pass 结束后该窗口内是否仍有节点带 `LAYOUT`（含冒泡）。
    pub layout_dirty: bool,
    /// pass 结束后该窗口内是否仍有节点 `!REUSABLE`。
    ///
    /// **P03 恒为 `true`**：`REUSABLE` 在 P03 只有清除者（`mark_*`），帧初重置属 P06 的
    /// `NodeTree::begin_frame`。因此除空窗口外该字段没有区分度。
    pub paint_dirty: bool,
    /// pass 结束后 `App::take_wake_request()` 会返回的值（**本报告不消费它**）。
    pub wake_requested: bool,
}

/// 引擎侧全部状态。字段全私有（`field_scoped_visibility_modifiers` 禁止字段级可见性）。
pub struct App {
    store: StateStore,
    graph: DependencyGraph,
    windows: ahash::HashMap<WindowId, Window>,
    eval_stack: EvalStack,
    wake_requested: bool,
    /// 下一个窗口 id（从 **1** 起单调 +1；`WindowId` 永不复用，0 非法）。
    next_window_id: u32,
    /// tokio 运行时的句柄（只保存；引擎内不得调用 `Handle::current()`，`clippy.toml` 已禁用）。
    runtime: tokio::runtime::Handle,
}

impl App {
    /// 建一个 headless 引擎（消费者：composition root，§9.2）。
    ///
    /// `runtime` 由调用方传入（`main` 建 runtime 后 `runtime.handle().clone()`）；引擎内不建 runtime，
    /// 也不做 `Handle::current()`（在 runtime 之外会 panic，且会让引擎依赖「调用线程恰好在 runtime 内」）。
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            store: StateStore::with_capacity(64, 16),
            graph: DependencyGraph::new(),
            windows: ahash::HashMap::default(),
            eval_stack: EvalStack::new(),
            wake_requested: false,
            next_window_id: 1,
            runtime,
        }
    }

    /// 本 App 绑定的 tokio 句柄（**借用**，不 clone）。
    ///
    /// 命名：不占用 `handle()` —— P09 会加 `App::handle() -> AppHandle`（那时是纯新增，零破坏）。
    #[must_use]
    pub fn runtime_handle(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    // ── 状态注册 ──────────────────────────────────────────────────────────────

    /// 建一个普通信号（id 稠密递增，条目永不删除）。
    pub fn signal<T: Clone + 'static>(&mut self, initial: T) -> Signal<T> {
        let id = self.store.push_owned(initial);
        Signal::new(id)
    }

    /// 建一个瞬态值（读不登记、写不失效）。
    pub fn transient<T: Clone + 'static>(&mut self, initial: T) -> Transient<T> {
        let id = self.store.push_transient(initial);
        Transient::new(id)
    }

    /// 注册一个派生值（**不**立即求值；环在运行期检测）。
    pub fn derived<T: Clone + 'static>(
        &mut self,
        compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
    ) -> Derived<T> {
        let boxed: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>> =
            Box::new(move |cx| Box::new(compute(cx)));
        let (id, _handle) = self.store.push_derived(boxed);
        Derived::new(id)
    }

    // ── 写路径 ────────────────────────────────────────────────────────────────

    /// 写一个信号。返回 `true` = 值变了（并已沿依赖图失效）；`false` = 值相等，**短路**。
    ///
    /// 返回值不表示「有节点被标脏」（只写派生时没有节点变脏）。
    ///
    /// **应用层契约**：`T` 必须是不可变值语义。若 `T` 内部可变（`Rc<RefCell<_>>`/`Cell`/`Mutex`）而
    /// `PartialEq` 只比较外层，则内部变化会被短路吞掉（不重绘）。见 S-35 / R9。
    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool {
        let id = signal.id();
        if self.store.owned_ref::<T>(id) == Some(&value) {
            return false;
        }
        self.store.set_owned(id, value);
        self.invalidate(id);
        true
    }

    /// 写一个瞬态值：**不**失效、**不**请求唤醒（滚动偏移/悬浮态，§3.7）。
    pub fn write_transient<T: 'static>(&mut self, transient: &Transient<T>, value: T) {
        self.store.set_transient(transient.id(), value);
    }

    /// 取走「需要一帧」的请求位（取走即清零）。
    pub fn take_wake_request(&mut self) -> bool {
        let requested = self.wake_requested;
        self.wake_requested = false;
        requested
    }

    // ── 窗口 ──────────────────────────────────────────────────────────────────

    /// 建一个窗口（headless：只建树与空场景）。`WindowId` 从 **1** 起单调 +1、永不复用
    /// （0 保留为非法句柄，见 `WindowId::from_raw`）。
    pub fn create_window(&mut self, options: WindowOptions) -> WindowId {
        let id = WindowId::from_raw(self.next_window_id);
        self.next_window_id = self.next_window_id.wrapping_add(1);
        self.windows.insert(id, Window::new(id, options));
        id
    }

    /// 只读窗口。
    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// 可写窗口（应用层建树的入口）。
    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    /// 关窗：精确退订该窗口全部节点的依赖，然后丢弃。返回 `false` = 窗口不存在。
    pub fn close_window(&mut self, id: WindowId) -> bool {
        let Some(window) = self.windows.remove(&id) else {
            return false;
        };
        for node in window.tree().order() {
            if let Some(deps) = window.tree().deps(*node) {
                for signal in deps {
                    self.graph.detach(
                        *signal,
                        Owner::Node {
                            window: id,
                            node: *node,
                        },
                    );
                }
            }
        }
        true
    }

    /// 遍历全部窗口（顺序不保证）并带出 `WindowId`。
    ///
    /// 带上 id 是必要的：帧是**按窗口**推进的（`refresh_styles(window)` / P06 的 paint），只有 `&Window`
    /// 拿不到「这是哪个窗口」（消费者：P09 的帧驱动、测试）。
    pub fn windows(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        self.windows.iter().map(|(id, window)| (*id, window))
    }

    // ── 帧内：样式求值 pass ────────────────────────────────────────────────────

    /// P03 的「样式求值 pass」：对指定窗口中**带 `PAINT` 且有 `bind_style`** 的节点求值。
    ///
    /// 它做四件事：求值闭包、差分替换依赖、写回 patch、按需升级 `LAYOUT` / 标 `A11Y`。
    /// **不产生任何 Scene 图元**（那是 P06 的 paint 阶段；两者共用本实现的前半段）。
    ///
    /// 返回 `None` = 窗口不存在（P03 的冻结错误表里没有「未知窗口」变体，`Option` 是唯一诚实的表达）。
    pub fn refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport> {
        let Self {
            store,
            graph,
            windows,
            eval_stack,
            wake_requested,
            ..
        } = self;
        let tree = windows.get_mut(&window)?.tree_mut();
        debug_assert!(eval_stack.is_empty(), "上一帧的派生求值栈泄漏");
        eval_stack.clear();

        let mut evaluated = 0usize;
        let mut index = 0usize;
        while index < tree.order().len() {
            let node = tree.order()[index];
            index += 1;

            let needs_eval = tree
                .get(node)
                .is_ok_and(|n| n.dirty().contains(DirtyFlags::PAINT));
            if !needs_eval {
                continue;
            }
            let old_patch = tree
                .get(node)
                .map_or_else(|_| StylePatch::default(), |n| n.style_patch().clone());
            let Some(compute) = tree.take_bind_style(node) else {
                // 闭包不见了 ⇒ 上次求值 **panic** 后没放回（`build` 之后必然有闭包）。
                // 那次 pass 在 `replace_deps` 之前就中断了，所以节点仍带着**上一次成功的** deps：
                // 它们是「幽灵边」—— 节点已不会再求值，但写这些信号仍会把它标脏。
                // 这里显式退订，把状态收敛成「静态节点」。
                let ghost_deps = tree.deps(node).unwrap_or(&[]);
                graph.replace_deps(Owner::Node { window, node }, ghost_deps, &[]);
                tree.set_deps(node, SmallVec::new());
                tree.clear_paint(node);
                continue;
            };

            let (values, derived) = store.split();
            let mut tracker = ReadTracker::new();
            // 作用域块结束即释放 `ReadCtx` 的借用（`ReadCtx` 不是 `Drop` 类型，不能用 `drop`）。
            let (patch, owner) = {
                let mut ctx = ReadCtx::new(
                    values,
                    derived,
                    graph,
                    &mut tracker,
                    &mut *eval_stack,
                    Owner::Node { window, node },
                );
                let patch = compute(&mut ctx);
                (patch, ctx.owner())
            };
            evaluated += 1;

            let old_deps = tree.deps(node).unwrap_or(&[]);
            graph.replace_deps(owner, old_deps, tracker.as_slice());
            tree.set_deps(node, tracker.into_inner());

            let layout_changed = patch.affects_layout || old_patch.affects_layout;
            let a11y_changed =
                patch.interactive != old_patch.interactive || patch.visible != old_patch.visible;

            tree.set_style_patch(node, patch);
            tree.put_bind_style(node, compute);
            if layout_changed {
                tree.mark_layout(node);
            }
            if a11y_changed {
                tree.mark_a11y(node);
            }
            tree.clear_paint(node);
        }

        Some(StylePassReport {
            evaluated,
            layout_dirty: tree.any_node(|n| n.dirty().contains(DirtyFlags::LAYOUT)),
            paint_dirty: tree.any_node(|n| !n.dirty().contains(DirtyFlags::REUSABLE)),
            wake_requested: *wake_requested,
        })
    }

    /// 沿依赖图失效：节点标脏（或经派生级联），有节点变脏则置唤醒位。
    fn invalidate(&mut self, signal: SignalId) {
        let Self {
            store,
            graph,
            windows,
            wake_requested,
            ..
        } = self;
        let mut worklist: SmallVec<[SignalId; 8]> = smallvec![signal];
        let mut marked = 0usize;

        while let Some(current) = worklist.pop() {
            let Some(owners) = graph.dependents_mut(current) else {
                continue;
            };
            owners.retain(|owner| match owner {
                Owner::Node { window, node } => windows
                    .get(window)
                    .is_some_and(|w| w.tree().get(*node).is_ok()),
                Owner::Derived(_) => true,
            });
            let empty = owners.is_empty();
            let snapshot: SmallVec<[Owner; 8]> = owners.iter().copied().collect();
            if empty {
                graph.remove_signal(current);
                continue;
            }

            for owner in snapshot {
                match owner {
                    Owner::Node { window, node } => {
                        let Some(target) = windows.get_mut(&window) else {
                            continue;
                        };
                        let affects_layout = target
                            .tree()
                            .get(node)
                            .is_ok_and(|n| n.style_patch().affects_layout);
                        let alive = if affects_layout {
                            target.tree_mut().mark_layout(node)
                        } else {
                            target.tree_mut().mark_paint(node)
                        };
                        if alive {
                            marked += 1;
                        }
                    }
                    Owner::Derived(derived) => {
                        if store.mark_derived_dirty(derived) {
                            worklist.push(store.derived_handle(derived));
                        }
                    }
                }
            }
        }

        if marked > 0 {
            *wake_requested = true;
        }
    }

    /// 仅测试：求值栈是否为空（验证 `EvalGuard` 在 panic 后仍然出栈）。
    #[cfg(test)]
    pub(crate) fn eval_stack_is_empty_for_test(&self) -> bool {
        self.eval_stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, panic::AssertUnwindSafe, rc::Rc};

    use super::App;
    use crate::{
        node::{
            builder::NodeBuilder,
            dependency::ReadCtx,
            dirty::DirtyFlags,
            id::NodeId,
            signal::{Derived, Signal, SignalId},
        },
        paint::style::StylePatch,
        window::{WindowId, WindowOptions},
    };

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(rt.handle().clone());
        (rt, app)
    }

    fn node_with<F>(app: &mut App, window: WindowId, bind: F) -> NodeId
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

    fn flags(app: &App, window: WindowId, node: NodeId) -> DirtyFlags {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
    }

    fn root_flags(app: &App, window: WindowId) -> DirtyFlags {
        let root = app.window(window).expect("窗口存在").tree().root();
        flags(app, window, root)
    }

    fn deps(app: &App, window: WindowId, node: NodeId) -> Vec<SignalId> {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .deps(node)
            .expect("节点存在")
            .to_vec()
    }

    fn clear_dirty(app: &mut App, window: WindowId) {
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .clear_all_dirty_for_test();
    }

    #[test]
    fn new_app_has_no_windows() {
        let (_rt, app) = new_app();

        assert_eq!(app.windows().count(), 0);
        assert!(app.window(WindowId::for_test(0)).is_none());
        assert!(app.eval_stack_is_empty_for_test());
        let joined = app.runtime_handle().spawn(async { 7u8 });
        drop(joined);
    }

    #[test]
    fn write_only_reaches_the_owning_window() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window_a = app.create_window(WindowOptions::default());
        let window_b = app.create_window(WindowOptions::default());
        let node_a = node_with(&mut app, window_a, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        let node_b = node_with(&mut app, window_b, |_cx| StylePatch::default());
        app.refresh_styles(window_a).expect("窗口存在");
        app.refresh_styles(window_b).expect("窗口存在");

        assert_eq!(
            node_a.index(),
            node_b.index(),
            "两窗口的 arena 索引相同 ⇒ 必须靠 Owner 的 window 消歧"
        );
        clear_dirty(&mut app, window_a);
        clear_dirty(&mut app, window_b);

        assert!(app.write(&signal, 1));

        assert!(flags(&app, window_a, node_a).contains(DirtyFlags::PAINT));
        assert!(
            !flags(&app, window_b, node_b).contains(DirtyFlags::PAINT),
            "不得命中另一个窗口的同号节点"
        );
    }

    #[test]
    fn write_marks_layout_when_patch_affects_layout() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            affects_layout: true,
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        clear_dirty(&mut app, window);

        assert!(app.write(&signal, 1));

        assert!(
            flags(&app, window, node).contains(DirtyFlags::LAYOUT),
            "affects_layout → 标 LAYOUT 而非只标 PAINT"
        );
        assert!(
            root_flags(&app, window).contains(DirtyFlags::LAYOUT),
            "必须冒泡"
        );
    }

    #[test]
    fn write_skips_stale_owner_without_panicking() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .remove(node)
            .expect("卸载成功");
        let _ = app.take_wake_request();

        assert!(app.write(&signal, 1), "值确实变了");
        assert!(
            !app.take_wake_request(),
            "没有存活的 owner → 不请求唤醒（陈旧 owner 已被 retain 剔除）"
        );
    }

    #[test]
    fn refresh_styles_only_reevaluates_dirty_nodes() {
        let (_rt, mut app) = new_app();
        let calls = Rc::new(Cell::new(0u32));
        let a = Rc::clone(&calls);
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            a.set(a.get() + 1);
            StylePatch {
                opacity: Some(cx.read(&signal) as f32),
                ..StylePatch::default()
            }
        });

        let first = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(first.evaluated, 1, "只有带 bind_style 的节点被求值");
        assert_eq!(calls.get(), 1);

        let second = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(second.evaluated, 0, "PAINT 已消费 → 不重复求值");
        assert_eq!(calls.get(), 1);

        assert!(app.write(&signal, 3));
        let third = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(third.evaluated, 1);
        assert_eq!(calls.get(), 2);
        assert_eq!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("存在")
                .style_patch()
                .opacity,
            Some(3.0)
        );
    }

    #[test]
    fn refresh_styles_is_idempotent() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });

        app.refresh_styles(window).expect("窗口存在");
        let deps_before = deps(&app, window, node);
        let patch_before = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("存在")
            .style_patch()
            .clone();

        let second = app.refresh_styles(window).expect("窗口存在");

        assert_eq!(second.evaluated, 0);
        assert_eq!(deps(&app, window, node), deps_before);
        assert_eq!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("存在")
                .style_patch(),
            &patch_before
        );
    }

    #[test]
    fn refresh_styles_escalates_layout_when_patch_affects_layout() {
        // 双向都要升级（`new || old`）：false→true 本帧开始变；true→false 要把尺寸收回来。
        let (_rt, mut app) = new_app();
        let resized: Signal<bool> = app.signal(false);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            affects_layout: cx.read(&resized),
            ..StylePatch::default()
        });

        let quiet = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(quiet.evaluated, 1);
        assert!(quiet.layout_dirty, "新建节点本身带 LAYOUT");

        clear_dirty(&mut app, window);
        assert!(app.write(&resized, true));
        let escalate = app.refresh_styles(window).expect("窗口存在");
        assert!(escalate.layout_dirty, "false → true 必须升级");
        assert!(flags(&app, window, node).contains(DirtyFlags::LAYOUT));
        assert!(root_flags(&app, window).contains(DirtyFlags::LAYOUT));

        clear_dirty(&mut app, window);
        assert!(app.write(&resized, false));
        let settle = app.refresh_styles(window).expect("窗口存在");
        assert!(
            settle.layout_dirty,
            "true → false 也必须升级（旧值声明过尺寸会变）"
        );
        assert!(flags(&app, window, node).contains(DirtyFlags::LAYOUT));
    }

    #[test]
    fn refresh_styles_unknown_window_returns_none() {
        let (_rt, mut app) = new_app();

        assert!(app.refresh_styles(WindowId::for_test(99)).is_none());
    }

    #[test]
    fn refresh_styles_updates_deps() {
        let (_rt, mut app) = new_app();
        let flag: Signal<bool> = app.signal(true);
        let a: Signal<u32> = app.signal(1);
        let b: Signal<u32> = app.signal(2);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            let use_a = cx.read(&flag);
            let value = if use_a { cx.read(&a) } else { cx.read(&b) };
            StylePatch {
                opacity: Some(value as f32),
                ..StylePatch::default()
            }
        });

        app.refresh_styles(window).expect("窗口存在");
        let first = deps(&app, window, node);
        assert!(first.contains(&flag.id()) && first.contains(&a.id()));
        assert!(!first.contains(&b.id()));

        let _ = app.take_wake_request();
        assert!(app.write(&flag, false));
        app.refresh_styles(window).expect("窗口存在");
        let second = deps(&app, window, node);
        assert!(
            second.contains(&b.id()) && !second.contains(&a.id()),
            "条件分支切换后必须退订 a、订阅 b"
        );

        clear_dirty(&mut app, window);
        let _ = app.take_wake_request();
        assert!(app.write(&a, 99));
        assert!(
            !flags(&app, window, node).contains(DirtyFlags::PAINT),
            "已退订的信号不得再标脏该节点"
        );
    }

    #[test]
    fn close_window_detaches_dependencies() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let _node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();

        assert!(app.close_window(window));
        assert!(app.window(window).is_none());
        assert!(app.write(&signal, 1), "写本身仍成功");
        assert!(
            !app.take_wake_request(),
            "关窗必须精确退订，不得再命中死节点"
        );
        assert!(!app.close_window(window), "重复关窗返回 false");
    }

    #[test]
    fn refresh_styles_marks_a11y_on_interactive_change() {
        let (_rt, mut app) = new_app();
        let enabled: Signal<bool> = app.signal(false);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            interactive: Some(cx.read(&enabled)),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        clear_dirty(&mut app, window);

        assert!(app.write(&enabled, true));
        app.refresh_styles(window).expect("窗口存在");

        assert!(
            flags(&app, window, node).contains(DirtyFlags::A11Y),
            "interactive 变化必须标 A11Y"
        );
    }

    #[test]
    fn eval_guard_pops_on_panic_and_cache_survives() {
        let (_rt, mut app) = new_app();
        let bad: Derived<f32> = app.derived(|_cx| -> f32 { panic!("派生闭包炸了") });
        let window = app.create_window(WindowOptions::default());
        // 第一个节点：本次求值会 panic（它的 `bind_style` 随后丢失 → 退化为静态节点）。
        let doomed = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read_derived(&bad).unwrap_or(0.0)),
            ..StylePatch::default()
        });
        // 第二个节点：求值面相同，但它没在 panic 里丢闭包 —— 用来证明后续读返回 `CyclicDerived` 而不是 panic。
        let survivor = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read_derived(&bad).unwrap_or(0.0)),
            ..StylePatch::default()
        });

        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = app.refresh_styles(window);
        }));
        assert!(outcome.is_err(), "闭包 panic 必须向上传播");
        assert!(
            app.eval_stack_is_empty_for_test(),
            "EvalGuard::drop 必须在 panic 展开时出栈"
        );

        let report = app.refresh_styles(window).expect("窗口仍在");
        assert_eq!(
            report.evaluated, 1,
            "panic 的那个节点闭包已丢（跳过），另一个节点仍被求值"
        );
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(survivor)
                .expect("存在")
                .style_patch()
                .opacity
                .is_some(),
            "read_derived 返回 CyclicDerived 后走了 unwrap_or，patch 仍被写入"
        );
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(doomed)
                .is_ok()
        );
    }

    #[test]
    fn write_isolates_sibling_nodes() {
        // D16/I3 的核心断言：一次写入只沿依赖图命中**读过它的**节点。
        let (_rt, mut app) = new_app();
        let signal_a: Signal<u32> = app.signal(0);
        let signal_b: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node_a = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal_a) as f32),
            ..StylePatch::default()
        });
        let node_b = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal_b) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let before = flags(&app, window, node_b);
        let _ = app.take_wake_request();

        assert!(app.write(&signal_a, 1));

        assert!(
            flags(&app, window, node_a).contains(DirtyFlags::PAINT),
            "读者必须被标脏"
        );
        assert_eq!(
            flags(&app, window, node_b),
            before,
            "非读者（含 REUSABLE）必须完全不变"
        );
        assert!(app.take_wake_request(), "有节点变脏 ⇒ 必须请求唤醒");
    }

    #[test]
    fn write_only_derived_does_not_wake() {
        // 只有派生失效、没有节点变脏 ⇒ 不该提交帧（I3「空闲不提交帧」）。
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(1);
        let derived: Derived<u32> = app.derived(move |cx| cx.read(&signal) * 2);
        let window = app.create_window(WindowOptions::default());

        assert!(app.write(&signal, 2));
        assert!(!app.take_wake_request(), "只有派生失效 → 不唤醒");

        let report = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(report.evaluated, 0, "没有节点读该派生");
        assert!(!report.wake_requested);
        assert!(app.write(&signal, 3));
        let _ = derived;
    }

    #[test]
    fn write_marks_across_windows() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window_a = app.create_window(WindowOptions::default());
        let window_b = app.create_window(WindowOptions::default());
        let node_a = node_with(&mut app, window_a, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        let node_b = node_with(&mut app, window_b, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window_a).expect("窗口存在");
        app.refresh_styles(window_b).expect("窗口存在");
        clear_dirty(&mut app, window_a);
        clear_dirty(&mut app, window_b);

        assert!(app.write(&signal, 9));

        assert!(flags(&app, window_a, node_a).contains(DirtyFlags::PAINT));
        assert!(flags(&app, window_b, node_b).contains(DirtyFlags::PAINT));
        assert!(app.take_wake_request());
    }

    #[test]
    fn create_window_ids_are_unique_and_monotonic() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let second = app.create_window(WindowOptions::default());
        let third = app.create_window(WindowOptions::default());

        assert_eq!(
            (first.to_bits(), second.to_bits(), third.to_bits()),
            (1, 2, 3),
            "WindowId 从 1 起单调递增（0 非法）"
        );
        assert_eq!(app.windows().count(), 3);
        for (id, window) in app.windows() {
            assert_eq!(id, window.id(), "windows() 带出的 id 必须与窗口自身一致");
            assert!(app.window(id).is_some());
        }
    }

    #[test]
    fn close_window_unknown_returns_false() {
        let (_rt, mut app) = new_app();

        assert!(!app.close_window(WindowId::for_test(42)));
    }

    #[test]
    fn bind_style_panic_clears_ghost_deps() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let calls = Rc::new(Cell::new(0u32));
        let counter = Rc::clone(&calls);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            counter.set(counter.get() + 1);
            let value = cx.read(&signal);
            assert!(counter.get() <= 1, "第二次求值故意 panic");
            StylePatch {
                opacity: Some(value as f32),
                ..StylePatch::default()
            }
        });
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(deps(&app, window, node), vec![signal.id()]);

        assert!(app.write(&signal, 4));
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = app.refresh_styles(window);
        }));
        assert!(outcome.is_err(), "panic 必须向上传播");
        assert_eq!(
            deps(&app, window, node),
            vec![signal.id()],
            "panic 打断了 replace_deps ⇒ 旧边仍在（幽灵边）"
        );

        let report = app.refresh_styles(window).expect("窗口仍在");
        assert_eq!(report.evaluated, 0, "闭包已丢 → 不算求值");
        assert!(deps(&app, window, node).is_empty(), "幽灵边必须被显式退订");

        let _ = app.take_wake_request();
        assert!(app.write(&signal, 5), "值仍然会变");
        assert!(!app.take_wake_request(), "退订后再写旧信号不得再命中该节点");
    }
}

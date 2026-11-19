//! 无障碍：动作 handler 表、诊断环、限流交付与快照捕获。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use smallvec::SmallVec;

use super::{A11Y_DIAGNOSTIC_CAPACITY, A11Y_THROTTLE, App};
use crate::{
    node::dependency::{Owner, ReadCtx, ReadTracker},
    window::{Window, WindowId},
};

// ── P11：无障碍（动作 / 诊断 / 交付） ──────────────────────────────────

/// 注册 a11y 动作的处理器（同 `A11yAction` 后写覆盖）。
///
/// 返回 `false` = 该动作**不能**注册：`A11yAction::Click` 必须落到 P10 的 `Action` 表
/// （`App::on_action`），让指针点击 / 键盘 Enter / AT 激活共享同一身份。
/// 消费者：`App::on_a11y_action`。
pub(crate) fn on_a11y_action(
    app: &mut App,
    action: crate::a11y::A11yAction,
    handler: impl FnMut(&mut crate::a11y::A11yActionCtx<'_>) + 'static,
) -> bool {
    if action == crate::a11y::A11yAction::Click {
        return false;
    }
    app.a11y_handlers.insert(action, Box::new(handler));
    true
}

/// 删除 a11y 动作的处理器（返回是否真的删了）。
/// 消费者：`App::off_a11y_action`。
pub(crate) fn off_a11y_action(app: &mut App, action: crate::a11y::A11yAction) -> bool {
    app.a11y_handlers.remove(&action).is_some()
}

/// a11y 诊断（与 `text_diagnostics` 同型；上限 `A11Y_DIAGNOSTIC_CAPACITY`，超出丢最旧）。
/// 消费者：`App::a11y_diagnostics`。
#[must_use]
pub(crate) fn a11y_diagnostics(app: &App) -> &[crate::a11y::A11yDiagnostic] {
    &app.a11y_diagnostics
}

/// 记录一条 a11y 诊断（唯一入口：计数与容量在这里统一）。
/// 消费者：`App::a11y_note`。
pub(crate) fn a11y_note(app: &mut App, diagnostic: crate::a11y::A11yDiagnostic) {
    // 去重：同一诊断会在**每次** a11y 交付里重复产生（例如同一个缺标签的交互节点）。
    // 旧实现每条都 push，64 槽环形缓冲很快被同一个问题的重复项占满，把真正的新问题挤出去，
    // 而且已经修好的旧诊断会长期残留（复审 P3-5）。
    if app.a11y_diagnostics.contains(&diagnostic) {
        return;
    }
    if app.a11y_diagnostics.len() >= A11Y_DIAGNOSTIC_CAPACITY {
        // 容量固定为 64：这里的一次 memmove 有界，不值得为它引入 VecDeque
        // （公开访问器返回 `&[A11yDiagnostic]`）。
        app.a11y_diagnostics.remove(0);
    }
    app.a11y_diagnostics.push(diagnostic);
}

/// 该动作是否注册了 handler（`a11y::action` 的分派判据）。
/// 消费者：`App::a11y_has_handler`。
pub(crate) fn a11y_has_handler(app: &App, action: crate::a11y::A11yAction) -> bool {
    app.a11y_handlers.contains_key(&action)
}

/// 摘出 handler（借用纪律：调用期间它不在表里）。
/// 消费者：`App::a11y_take_handler`。
pub(crate) fn a11y_take_handler(
    app: &mut App,
    action: crate::a11y::A11yAction,
) -> Option<Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>> {
    app.a11y_handlers.remove(&action)
}

/// 放回 handler。
/// 消费者：`App::a11y_put_handler`。
pub(crate) fn a11y_put_handler(
    app: &mut App,
    action: crate::a11y::A11yAction,
    handler: Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
) {
    app.a11y_handlers.insert(action, handler);
}

/// 该窗口本轮是否有 a11y 工作（O(1)；平台泵每轮用它过滤 `update_if_active`）。
/// 消费者：`App::a11y_pending`。
///
/// 三条不节流旁路（`A11Y_THROTTLE` 只约束**已接入后**的高频数据增量）：
/// - **焦点变化**（R13-5）：读屏用户的"位置感"绑定在焦点反馈的时效上；
/// - **消费端重置**（E1）：AT 重新激活 / adapter 停用后 AccessKit 停在 `Pending`，在收到全树
///   之前 AT 看到的是空树；此时也没有脏位可供到期补发，必须立刻交付；
/// - **未交付批次**（E5）：分批首树 / DPI 全量重建只发了一部分时必须连续推进 —— 否则
///   40k 节点 ≈ 20 批 × 100 ms 节流 ≈ 2 s 的首屏可访问性延迟（审查 §3.10 P1）。
#[must_use]
pub(crate) fn a11y_pending(app: &App, window: WindowId) -> bool {
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    if !target.a11y_pending() {
        return false;
    }
    target.a11y_focus_pending()
        || target.a11y_consumer_epoch_pending()
        || target.a11y_backlog_pending()
        || app
            .a11y_delivered
            .get(&window)
            .is_none_or(|last| app.clock.saturating_duration_since(*last) >= A11Y_THROTTLE)
}

/// 是否有窗口「a11y 脏但正被限流」——平台泵据此安排**到期补发**唤醒。
///
/// 没有这条，被限流挡下的最后一次变化永远不会交付：脏位留着，但没有任何机制在
/// `A11Y_THROTTLE` 之后把泵叫醒（复审 P1-4）。
/// 消费者：`App::a11y_retry_pending`。
pub(crate) fn a11y_retry_pending(app: &App) -> bool {
    app.windows.keys().any(|window| {
        app.windows.get(window).is_some_and(Window::a11y_pending)
            && app
                .a11y_delivered
                .get(window)
                .is_some_and(|last| app.clock.saturating_duration_since(*last) < A11Y_THROTTLE)
    })
}

/// 产出一笔 a11y 交付（消费 `A11Y` 反向集合与 `full_pending`）。
///
/// **必须在 `accesskit_windows::Adapter::update_if_active` 的闭包里调用**（adapter 不活跃时
/// 闭包不执行 ⇒ 零开销），或由 `test-support` 的捕获路径调用。
/// 消费者：`App::a11y_take`。
pub(crate) fn a11y_take(app: &mut App, window: WindowId) -> Option<crate::a11y::A11yTreeSnapshot> {
    let mut diagnostics = Vec::new();
    let snapshot = {
        let App { windows, .. } = app;
        windows.get_mut(&window)?.a11y_take(&mut diagnostics)
    };
    for diagnostic in diagnostics {
        app.a11y_note(diagnostic);
    }
    if snapshot.is_some() {
        app.a11y_delivered.insert(window, app.clock);
    }
    snapshot
}

/// 只重算 `bind_a11y` 的 pass（R13-6 的 `refresh_a11y_bindings`）。
///
/// **不产生任何像素工作**：不标 `BIND`/`PAINT`、不清 `REUSABLE`、不产生 damage。消费者：
/// `App::refresh_styles`（每帧的样式 pass 之后调用一次；空队列时 O(1) 直接返回），因此
/// 公开 API 的语义仍是"全部动态属性都已重算"。
///
/// 为什么单独成 pass：只有 `bind_a11y` 的节点被写信号时，整条帧管线里**唯一**必需的步骤就是
/// 重算 patch；把它从 `refresh_styles` 的 `dirty_bind` 通道里解耦后，这类帧可以整段跳过
/// 文本/布局/paint/编码（判据见 `Window::a11y_only_frame`），而 TreeUpdate 照常在平台泵里
/// 投递 —— 节流（`A11Y_THROTTLE`）因此只约束投递，不再被"整帧 pass"摊薄。
///
/// 依赖边处理与 `refresh_styles` 完全一致（同一 `Owner`、同一差分替换）：一个节点只有一份
/// 依赖边集，`bind_style` / `bind_text` / `bind_a11y` 三条动态通道共享它。
pub(crate) fn refresh_a11y_bindings(app: &mut App, window: WindowId) {
    if !app
        .windows
        .get(&window)
        .is_some_and(Window::a11y_binding_work)
    {
        return;
    }
    let App {
        store,
        graph,
        windows,
        eval_stack,
        ..
    } = app;
    debug_assert!(eval_stack.is_empty(), "上一帧的派生求值栈泄漏");
    eval_stack.clear();
    let Some(target) = windows.get_mut(&window) else {
        return;
    };
    let nodes = target.a11y_take_bindings();
    let tree = target.tree_mut();
    for node in nodes.iter().copied() {
        if tree.get(node).is_err() {
            continue;
        }
        let Some(bind) = tree.take_bind_a11y(node) else {
            // 闭包不见了 ⇒ 上次求值 **panic** 后没放回（与 `refresh_styles` 的同一分支同义）。
            // 那次 pass 在 `replace_deps` 之前就中断了，节点仍带着上一次成功的 deps：它们是
            // 「幽灵边」—— 节点已不会再求值，但写这些信号仍会把它标脏。这里显式退订。
            let ghost_deps = tree.deps(node).unwrap_or(&[]);
            graph.replace_deps(Owner::Node { window, node }, ghost_deps, &[]);
            tree.set_deps(node, SmallVec::new());
            continue;
        };
        let (values, derived) = store.split();
        let mut tracker = ReadTracker::new();
        // 作用域块结束即释放 `ReadCtx` 的借用（`ReadCtx` 不是 `Drop` 类型，不能用 `drop`）。
        let (value, owner) = {
            let mut ctx = ReadCtx::new(
                values,
                derived,
                graph,
                &mut tracker,
                &mut *eval_stack,
                Owner::Node { window, node },
            );
            (bind(&mut ctx), ctx.owner())
        };
        let old_deps = tree.deps(node).unwrap_or(&[]);
        graph.replace_deps(owner, old_deps, tracker.as_slice());
        tree.set_deps(node, tracker.into_inner());
        let previous = tree
            .a11y_of(node)
            .map_or_else(crate::a11y::A11yPatch::default, |slot| slot.patch.clone());
        if value != previous {
            tree.set_a11y_patch(node, value);
            // 只标 `A11Y`：不产生视觉 damage、不清 `REUSABLE`（P03 §9.3 表第 9 行）。
            tree.mark_a11y(node);
        }
        tree.put_bind_a11y(node, bind);
    }
    target.a11y_recycle_bindings(nodes);
}

/// 打开/关闭某个窗口的 a11y 快照捕获（`TestApp` 专用）。
/// 消费者：`App::set_a11y_capture`。
#[cfg(feature = "test-support")]
pub(crate) fn set_a11y_capture(app: &mut App, window: WindowId, on: bool) {
    if let Some(target) = app.windows.get_mut(&window) {
        target.set_a11y_capture(on);
    }
}

/// 该窗口的捕获是否打开。
/// 消费者：`App::attach_a11y`。
#[cfg(feature = "test-support")]
pub(crate) fn attach_a11y(
    app: &mut App,
    window: WindowId,
    snapshot: crate::a11y::A11yTreeSnapshot,
) {
    if let Some(target) = app.windows.get_mut(&window) {
        target.attach_a11y(snapshot);
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use smol_str::SmolStr;

    use crate::{
        a11y::A11yPatch,
        app::App,
        math::{Rect, Scale, Size},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal},
        paint::{
            color::Color,
            style::{PaintSpec, StylePatch},
        },
        window::{FrameOutcome, WindowId, WindowOptions},
    };

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    /// 夹具：一个窗口 + `10` 个静态实色 box（有 lane）+ 一个**只**绑 `bind_a11y` 的实色 box。
    ///
    /// 返回 `(runtime, app, window, label_signal, label_node)`；`runtime` 必须保活（`App` 只持
    /// `Handle`）。
    fn fixture() -> (
        tokio::runtime::Runtime,
        App,
        WindowId,
        Signal<SmolStr>,
        NodeId,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("a11y-only-cost"),
            size: Size::new(320.0, 240.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        for index in 0..10 {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 6.0, 40.0, 5.0))
                .paint(white())
                .build(app.window_mut(window).expect("window").tree_mut(), root)
                .expect("static box");
        }
        let label_signal = app.signal(SmolStr::new_static("旧的"));
        let source = label_signal;
        let label = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 80.0, 120.0, 20.0))
            .paint(white())
            .bind_a11y(move |cx| A11yPatch {
                label: Some(cx.read(&source).clone()),
                ..A11yPatch::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("a11y label");
        (runtime, app, window, label_signal, label)
    }

    /// R13-6 的验收：**只改一个节点的 a11y label** 不得触发重绘。
    ///
    /// 判据（全部确定性，不依赖机器负载）：
    /// 1. 本帧是 `Idle`（不提交，`FrameIndex` 不变）；
    /// 2. `record.a11y_only == true` 且 `record.repainted == false`；
    /// 3. `damage` 空、`painted_lanes == 0`、`encoded_lanes == 0`、paint pass 没有访问任何 lane；
    /// 4. `stats.a11y_only_frames == 1`（a11y-only 快速路径真的在跑，不是"碰巧没画"）；
    /// 5. a11y 树**确实更新了**（TreeUpdate 只重建，不重绘）。
    ///
    /// 变异守卫：把 `App::invalidate` 的 a11y-only 分支退回「一律重绘」（标 `PAINT`）⇒ 第 2/3
    /// 条变红；删掉 a11y-only 快速路径 ⇒ 第 4 条变红。
    #[test]
    fn an_a11y_only_change_does_not_repaint() {
        let (_runtime, mut app, window, label_signal, label) = fixture();
        // 首帧：全量（新节点带 LAYOUT|PAINT|BIND）⇒ 正常提交；随后把全树交付消费掉。
        let first = app.frame(window).expect("window").expect("first frame");
        assert!(
            matches!(first, FrameOutcome::Committed(_)),
            "首帧必须提交（实际 {first:?}）"
        );
        assert!(app.a11y_take(window).is_some(), "首笔 a11y 交付必为全树");
        let before = app.window(window).expect("window").frame_index();

        app.write(&label_signal, SmolStr::new_static("新的"));
        assert!(
            app.window(window).expect("window").a11y_binding_work(),
            "写 a11y 绑定后必须登记待重算（不得走像素通道）"
        );
        assert!(
            app.window(window).expect("window").a11y_pending(),
            "写 a11y 绑定后必须有待投递的 a11y 工作（限流只管投递时机，不管 patch 重算）"
        );

        let outcome = app.frame(window).expect("window").expect("a11y-only frame");
        assert!(
            matches!(outcome, FrameOutcome::Idle),
            "a11y-only 帧不得提交像素（实际 {outcome:?}）"
        );

        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("record");
        assert_eq!(
            record.frame_index, before,
            "a11y-only 帧不得推进 FrameIndex"
        );
        assert!(record.a11y_only, "本帧必须被判定为 a11y-only");
        assert!(!record.repainted, "a11y-only 帧不得发生任何重绘");
        assert!(!record.rendered, "a11y-only 帧不提交像素");
        assert!(record.frame.is_none(), "不得产生 RenderedFrame");
        assert!(record.damage.is_empty(), "a11y-only 帧不得产生 damage");
        assert_eq!(record.stats.painted_lanes, 0, "不得重建任何 lane");
        assert_eq!(record.stats.encoded_lanes, 0, "不得重编码任何 lane");
        assert!(
            record.paint_log.lanes.is_empty(),
            "paint pass 不得访问任何 lane（实际 {:?}）",
            record.paint_log.lanes
        );
        assert!(record.dirty_nodes.is_empty(), "不得有节点被重绘");
        assert_eq!(
            record.stats.a11y_only_frames, 1,
            "a11y-only 快速路径必须记一笔（否则帧可能只是碰巧不必画）"
        );

        let snapshot = app.a11y_take(window).expect("a11y 交付必须发生");
        assert!(
            !snapshot.full,
            "第二笔必须是增量（只重发变化节点 + 祖先链）"
        );
        assert_eq!(
            snapshot.node_for(label).expect("label 节点").label_text(),
            Some("新的"),
            "a11y 树必须拿到新 label（不重绘 ≠ 不交付）"
        );
    }

    /// R13-6 的配套约束：**有像素工作的帧**里，`bind_a11y` 也必须在同一帧重算。
    ///
    /// 同一个信号同时被一个 `bind_style` 节点和一个只绑 `bind_a11y` 的节点读取：帧仍是
    /// 绘制帧（按 PAINT 走完整管线），但 a11y 绑定 pass 也必须在 paint 之前跑完 —— 否则
    /// a11y 交付会落后一帧（读屏读到旧 label）。
    #[test]
    fn a_paint_frame_also_refreshes_a11y_bindings() {
        let (_runtime, mut app, window, label_signal, label) = fixture();
        let root = app.window(window).expect("window").tree().root();
        let shared = label_signal;
        let styled = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 110.0, 40.0, 20.0))
            .paint(white())
            .bind_style(move |cx| StylePatch {
                opacity: Some(if cx.read(&shared).as_str() == "新的" {
                    0.5
                } else {
                    1.0
                }),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("styled box");
        let first = app.frame(window).expect("window").expect("first frame");
        assert!(matches!(first, FrameOutcome::Committed(_)));
        let _ = app.a11y_take(window).expect("首笔 a11y 交付");

        app.write(&label_signal, SmolStr::new_static("新的"));
        let outcome = app.frame(window).expect("window").expect("paint frame");
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "有 style 绑定的同一批写必须落成绘制帧（实际 {outcome:?}）"
        );
        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("record");
        assert!(!record.a11y_only, "像素帧不得被误判成 a11y-only");
        assert_eq!(
            record.stats.a11y_only_frames, 0,
            "像素帧不得计入 a11y-only 帧"
        );

        let snapshot = app.a11y_take(window).expect("a11y 交付");
        assert_eq!(
            snapshot.node_for(label).expect("label 节点").label_text(),
            Some("新的"),
            "同一帧里 bind_a11y 必须已重算"
        );
        let _ = styled;
    }

    /// E1：消费端重置（AT 重新激活 / adapter 停用）必须**立即**交付全树，不受 `A11Y_THROTTLE`。
    ///
    /// 变异守卫：删掉 `a11y_pending` 的 `a11y_consumer_epoch_pending()` 旁路 ⇒ 第一个
    /// `assert!(app.a11y_pending(...))` 变红（时钟未推进，节流窗口内普通脏位不会交付）。
    #[test]
    fn a_consumer_epoch_reset_bypasses_the_a11y_throttle() {
        let (_runtime, mut app, window, _label_signal, label) = fixture();
        let _ = app.frame(window).expect("window").expect("first frame");
        assert!(app.a11y_take(window).is_some(), "首笔 a11y 交付必为全树");

        // 首笔刚交付：时钟未推进，普通脏位落在 100 ms 节流窗口内。
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(label);
        assert!(
            !app.a11y_pending(window),
            "限流窗口内的普通脏位不得立即交付（否则旁路测试没有意义）"
        );

        // 消费端重置：不推进时钟也必须立即交付，且下一笔是全树。
        let epoch = app.window(window).expect("window").a11y_consumer_epoch();
        epoch.bump();
        assert!(
            app.a11y_pending(window),
            "消费端重置必须走不节流旁路（AT 此时看到空树，等 100 ms 没有意义）"
        );
        let snapshot = app.a11y_take(window).expect("重置后的第一笔必须产出");
        assert!(snapshot.full, "重置后的第一笔必须是全树");
    }
}

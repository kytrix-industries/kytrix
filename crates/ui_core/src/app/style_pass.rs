//! 样式求值 pass、脏标记失效、动作 handler 表与键盘绑定表。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use smallvec::{SmallVec, smallvec};

use super::{App, StylePassReport};
use crate::{
    event::{Action, KeyBinding, KeyCode, Modifiers},
    interaction::ActionCtx,
    node::{
        dependency::{Owner, ReadCtx, ReadTracker},
        dirty::DirtyFlags,
        signal::SignalId,
    },
    paint::style::StylePatch,
    text::{TextContent, TextStyle},
    window::WindowId,
};

// ── P10：动作（Action）与键盘绑定 ──────────────────────────────────────────

/// 注册/替换一个动作的处理器（同 `Action` 后写覆盖）。
/// 消费者：`App::on_action`。
pub(crate) fn on_action(
    app: &mut App,
    action: Action,
    handler: impl FnMut(&mut ActionCtx<'_>) + 'static,
) {
    app.handlers.insert(action, Box::new(handler));
}

/// 删除处理器（返回是否真的删了）。
/// 消费者：`App::off_action`。
pub(crate) fn off_action(app: &mut App, action: &Action) -> bool {
    app.handlers.remove(action).is_some()
}

/// 仅测试（`test-support`）：某个动作当前是否注册了处理器。
///
/// 组件 `release` 清理的**直接证据面**：泄漏的 handler 不一定会 panic（`ctx.update` 对死
/// 句柄是 no-op、滚动条 handler 只写引擎状态），所以「重放输入不 panic」不足以证明表项被摘掉。
/// 消费者：`App::has_action_handler`（base_ui release 测试）。
#[cfg(feature = "test-support")]
pub(crate) fn has_action_handler(app: &App, action: &Action) -> bool {
    app.handlers.contains_key(action)
}

/// 仅测试（`test-support`）：某个 `(window, scope)` 上剩余的焦点作用域化键绑定条数。
///
/// `App::unbind_scope_keys` 的直接证据面（树 release）：只删除绑定时重放按键不会 panic
/// （handler 已摘 ⇒ 不消费、回落默认行为），因此必须观察表项本身。
/// 消费者：`App::scoped_key_binding_count`（base_ui `Tree::release` 测试）。
#[cfg(feature = "test-support")]
pub(crate) fn scoped_binding_count(
    app: &App,
    window: WindowId,
    scope: crate::node::id::NodeId,
) -> usize {
    app.scoped_bindings
        .iter()
        .filter(|binding| binding.window == window && binding.scope == scope)
        .count()
}

/// 添加/替换一条键盘绑定（同 `(key, modifiers)` 后写覆盖）。
/// 消费者：`App::bind_key`。
pub(crate) fn bind_key(app: &mut App, binding: KeyBinding) {
    app.bindings.retain(|existing| {
        !(existing.key == binding.key && existing.modifiers == binding.modifiers)
    });
    app.bindings.push(binding);
}

/// 添加/替换一条焦点作用域化的键盘绑定（同 `(window, scope, key, modifiers)` 后写覆盖）。
///
/// 注册前校验窗口与 `scope` 节点：**永不命中**的绑定必须在注册期拒绝（否则表项会一直留到
/// 窗口关闭，见 `App::bind_key_scoped` 的清理契约）。
/// 消费者：`App::bind_key_scoped`。
pub(crate) fn bind_key_scoped(
    app: &mut App,
    window: crate::window::WindowId,
    scope: crate::node::id::NodeId,
    binding: KeyBinding,
) -> Result<(), crate::error::EngineError> {
    use crate::error::EngineError;
    let Some(target) = app.windows.get(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    // `NodeId` 携带窗口身份：另一窗口的节点在这里确定性失败（`StaleNode`），不会串窗命中。
    target.tree().get(scope)?;
    app.scoped_bindings.retain(|existing| {
        !(existing.window == window
            && existing.scope == scope
            && existing.binding.key == binding.key
            && existing.binding.modifiers == binding.modifiers)
    });
    app.scoped_bindings.push(super::ScopedKeyBinding {
        window,
        scope,
        binding,
    });
    Ok(())
}

/// 删除一条 scoped 绑定（返回是否真的删了）。
/// 消费者：`App::unbind_key_scoped`。
pub(crate) fn unbind_key_scoped(
    app: &mut App,
    window: crate::window::WindowId,
    scope: crate::node::id::NodeId,
    key: KeyCode,
    modifiers: Modifiers,
) -> bool {
    let before = app.scoped_bindings.len();
    app.scoped_bindings.retain(|existing| {
        !(existing.window == window
            && existing.scope == scope
            && existing.binding.key == key
            && existing.binding.modifiers == modifiers)
    });
    app.scoped_bindings.len() != before
}

/// 删除某个 scope 在某个窗口上的**全部** scoped 绑定，返回删除条数。
/// 消费者：`App::unbind_scope_keys`（组件 `release` 不需要复述自己绑定过哪些键）。
pub(crate) fn unbind_scope_keys(
    app: &mut App,
    window: crate::window::WindowId,
    scope: crate::node::id::NodeId,
) -> usize {
    let before = app.scoped_bindings.len();
    app.scoped_bindings
        .retain(|existing| !(existing.window == window && existing.scope == scope));
    before - app.scoped_bindings.len()
}

/// 删除一条键盘绑定（返回是否真的删了）。
/// 消费者：`App::unbind_key`。
pub(crate) fn unbind_key(app: &mut App, key: KeyCode, modifiers: Modifiers) -> bool {
    let before = app.bindings.len();
    app.bindings
        .retain(|binding| !(binding.key == key && binding.modifiers == modifiers));
    app.bindings.len() != before
}

// ── 帧内：样式求值 pass ────────────────────────────────────────────────────

/// P03 的「样式求值 pass」：对指定窗口中**带 `BIND`** 的节点求值。
///
/// 它做四件事：求值闭包、差分替换依赖、写回 patch、按需升级 `LAYOUT` / 标 `A11Y`。
/// **不产生任何 Scene 图元**（那是 P06 的 paint 阶段；两者共用本实现的前半段）。
///
/// 只遍历 `dirty_bind` 反向集合（R03-1 / R03-9）：旧实现每帧遍历全序 `order` 并对每个节点
/// 查 `PAINT` 位 —— 既是 O(N)，又把"重算动态属性"和"重建 lane"压在同一个位上。
///
/// # R03-15 的结论（复核记录，防再被误判为"未修"）
///
/// 台账把它记为「每 pass 整体替换 deps + 每节点 clone old_patch」，但按代码逐项核对：
/// - `DependencyGraph::replace_deps` **本身就是差量**：只对「旧有新无」退订、「新有旧无」订阅，
///   相同的边既不删也不重加（`node/dependency.rs` 的同名方法与 `replace_deps_*` 单测）；
/// - `tree.set_deps` 换的是 `SmallVec<[SignalId; 8]>`（内联 8），常规读者数下**零堆分配**；
/// - `style_patch().clone()` 克隆的是 `StylePatch`：字段全是 `Copy`（`Option<Color>`/`Option<Border>`/
///   `Option<f32>`/`Option<bool>`/`Option<Cursor>`/`bool`），clone 就是一次 ~40 B 的 memcpy。
///
/// 唯一真实的剩余成本是 `ReadTracker::record` 的线性查重（典型 1–5 个读者）—— 已作为
/// **R03-5 的显式取舍**登记（测量驱动，不在 1–5 读者规模上引入 HashSet/位图）。
/// 因此本条**不再有一次"整表重建"级别的开销可消**；如未来出现 >8 读者的节点，先动的是 R03-5。
///
/// 返回 `None` = 窗口不存在（P03 的冻结错误表里没有「未知窗口」变体，`Option` 是唯一诚实的表达）。
/// P11 追加：`bind_a11y` 与另两条动态通道同批求值（同一 `ReadCtx` / `ReadTracker` / 依赖边集）。
/// **只有** `bind_a11y` 的节点不在这里求值（R13-6）：它们走 `refresh_a11y_bindings`，不标
/// `BIND`/`PAINT`（帧可以整段跳过像素管线）。
/// 消费者：`App::refresh_styles`。
pub(crate) fn refresh_styles(app: &mut App, window: WindowId) -> Option<StylePassReport> {
    let App {
        store,
        graph,
        windows,
        text,
        eval_stack,
        ..
    } = app;
    let window_state = windows.get_mut(&window)?;
    let tree = window_state.tree_mut();
    debug_assert!(eval_stack.is_empty(), "上一帧的派生求值栈泄漏");
    eval_stack.clear();

    let mut evaluated = 0usize;
    let mut text_evaluated = 0usize;
    let mut text_changed = 0usize;
    let nodes = tree.take_dirty_bind();
    for node in nodes.iter().copied() {
        // 集合的成员资格是权威判据；防御性再确认一次位（`take` 之后位可能已被消费）。
        let needs_eval = tree
            .get(node)
            .is_ok_and(|n| n.dirty().contains(DirtyFlags::BIND));
        if !needs_eval {
            tree.clear_bind(node);
            continue;
        }
        let old_patch = tree
            .get(node)
            .map_or_else(|_| StylePatch::default(), |n| n.style_patch().clone());
        let compute = tree.take_bind_style(node);
        let bind_text = tree.take_bind_text(node);
        let bind_a11y = tree.take_bind_a11y(node);
        if compute.is_none() && bind_text.is_none() && bind_a11y.is_none() {
            // 闭包不见了 ⇒ 上次求值 **panic** 后没放回（`build` 之后必然有闭包）。
            // 那次 pass 在 `replace_deps` 之前就中断了，所以节点仍带着**上一次成功的** deps：
            // 它们是「幽灵边」—— 节点已不会再求值，但写这些信号仍会把它标脏。
            // 这里显式退订，把状态收敛成「静态节点」。
            let ghost_deps = tree.deps(node).unwrap_or(&[]);
            graph.replace_deps(Owner::Node { window, node }, ghost_deps, &[]);
            tree.set_deps(node, SmallVec::new());
            tree.clear_bind(node);
            continue;
        }

        let (values, derived) = store.split();
        let mut tracker = ReadTracker::new();
        // 作用域块结束即释放 `ReadCtx` 的借用（`ReadCtx` 不是 `Drop` 类型，不能用 `drop`）。
        let (patch, text_value, a11y_value, owner) = {
            let mut ctx = ReadCtx::new(
                values,
                derived,
                graph,
                &mut tracker,
                &mut *eval_stack,
                Owner::Node { window, node },
            );
            let patch = compute
                .as_ref()
                .map_or_else(StylePatch::default, |compute| compute(&mut ctx));
            let text_value = bind_text.as_ref().map(|bind| bind(&mut ctx));
            let a11y_value = bind_a11y
                .as_ref()
                .map_or_else(crate::a11y::A11yPatch::default, |bind| bind(&mut ctx));
            (patch, text_value, a11y_value, ctx.owner())
        };
        if compute.is_some() {
            evaluated += 1;
        }
        if text_value.is_some() {
            text_evaluated += 1;
        }

        let old_deps = tree.deps(node).unwrap_or(&[]);
        graph.replace_deps(owner, old_deps, tracker.as_slice());
        tree.set_deps(node, tracker.into_inner());

        if let Some(compute) = compute {
            let layout_changed = patch.affects_layout || old_patch.affects_layout;
            let a11y_changed =
                patch.interactive != old_patch.interactive || patch.visible != old_patch.visible;
            tree.set_style_patch(node, patch);
            tree.put_bind_style(node, compute);
            if layout_changed {
                tree.mark_layout(node);
            }
            if a11y_changed {
                // E15：`visible` / `interactive` 是**沿祖先链生效**的 a11y 输入 —— `focusable`
                // （隐式 `Focus` 能力）与 `is_hidden` 都向上看祖先。祖先切换可见/可交互后，
                // 后代的 `Focus` / `Click` 能力会翻转，但后代自己没有脏位；只标本节点时增量笔
                // 与全树分叉（P2-4c2 / P2-4d2 的「增量缺 Focus」证据：浮层根 `visible` 一翻，
                // 面板里的 `Tab` / `List` 容器在增量笔里仍是没有 Focus 的旧记录）。
                // 整棵子树标 `A11Y`：只重发 a11y 快照字段，不产生像素工作、不清 `REUSABLE`。
                tree.mark_subtree_a11y(node);
            }
        }
        let mut text_changed_here = false;
        if let Some(bind) = bind_text {
            tree.put_bind_text(node, bind);
        }
        if let Some(value) = text_value {
            let style = tree
                .text_content(node)
                .map_or_else(TextStyle::default, |content| content.style.clone());
            let changed = tree
                .text_content(node)
                .is_none_or(|content| content.text != value);
            if changed {
                // v2.1 E2（`§11.2`）：只换整段 text/style，**保留** runs 与 inline_boxes
                // （静态载荷不被动态文本清空）。新文本更短时按新长度裁剪 / 丢弃越界 runs，
                // 并压诊断 —— 不静默改变样式归属（`clip_runs_to_text` 的返回值就是被改动的
                // run 数）。
                let mut content = tree
                    .text_content(node)
                    .cloned()
                    .unwrap_or_else(|| TextContent::new(value.clone(), style.clone()));
                content.text = value;
                content.style = style;
                let clipped_runs = content.clip_runs_to_text();
                if clipped_runs > 0 {
                    text.push_diagnostics(vec![crate::text::TextDiagnostic::TextRunClipped {
                        runs: clipped_runs,
                    }]);
                }
                tree.set_text_content(node, content);
                text_changed += 1;
                text_changed_here = true;
            }
        }
        // P11：`bind_a11y` 的差分与写回（与 `bind_style` 同一个依赖集，见方法文档）。
        if let Some(bind) = bind_a11y {
            let previous = tree
                .a11y_of(node)
                .map_or_else(crate::a11y::A11yPatch::default, |slot| slot.patch.clone());
            if a11y_value != previous {
                tree.set_a11y_patch(node, a11y_value);
                // 只标 `A11Y`：不产生视觉 damage、不清 `REUSABLE`（P03 §9.3 表第 9 行）。
                tree.mark_a11y(node);
            }
            tree.put_bind_a11y(node, bind);
        }
        // `BIND` 的唯一清除点：本 pass 求值完即消费（`PAINT` 不归这里管）。
        tree.clear_bind(node);
        if text_changed_here {
            // 文本变了 ⇒ 必须重画（`PAINT` 是 paint pass 的通道，本 pass 只登记）。
            tree.mark_paint(node);
            // 文本节点的可访问名/值就是文本内容：内容变了 a11y 必须重发。只标 PAINT 时，
            // 新字符串与旧串度量相同（例如 "70%" → "80%"）不会触发 LAYOUT，AT 会读到旧值。
            tree.mark_a11y(node);
        }
    }
    tree.recycle(nodes);

    let layout_dirty = tree.has_layout_work();
    let paint_dirty = tree.has_paint_work();
    Some(StylePassReport {
        evaluated,
        // O(1)：读反向集合长度，不再为被丢弃的诊断字段做两次全树扫描（R03-1 / R01-4）。
        layout_dirty,
        paint_dirty,
        wake_requested: app.wake_requested,
        text_evaluated,
        text_changed,
    })
}

/// 沿依赖图失效：节点标脏（或经派生级联），有节点变脏则置唤醒位。
/// 消费者：`App::invalidate`。
pub(crate) fn invalidate(app: &mut App, signal: SignalId) {
    let App {
        store,
        graph,
        windows,
        ..
    } = app;
    let mut worklist: SmallVec<[SignalId; 8]> = smallvec![signal];
    let mut marked = 0usize;

    while let Some(current) = worklist.pop() {
        let Some(owners) = graph.dependents_mut(current) else {
            continue;
        };
        // R01-5 的复核结论（代码注释即登记，防止再被当成"漏修"）：这里的两处开销都**不能删**：
        //
        // 1. `retain` 的逐 owner 存活检查是**惰性剔除**的唯一实现。死 owner（节点已卸载、arena
        //    槽位已复用）只能在这里被发现 —— 引擎的卸载路径不逐信号摘边（`remove_node` 只解自己
        //    那一层的边，`close_window` 是唯一的全量退订点）。删掉它，反查表会随节点回收无界增长，
        //    且写一次信号就要为死 owner 付出一次 `windows.get`。若将来改为**卸载时立刻摘边**，
        //    这里才能删；那是 R03-6 槽位回收的配套改动，不是本 pass 的局部优化。
        // 2. `snapshot` 是 `SmallVec<[Owner; 8]>`：读者 ≤8 时**纯栈上**，无堆分配；超过 8 才
        //    一次分配。它存在的原因是借用（`dependents_mut` 的可变借用不能在遍历中再次借用
        //    `graph` 去做级联），不是"每次都要分配一份拷贝"。
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
                    // P11 / R13-6：只有 `bind_a11y` 的节点（没有视觉绑定）被写信号时，**不得**
                    // 进帧的像素通道：登记到 a11y 桥的待重算队列（不标 `BIND`/`PAINT`）。
                    // 下一帧的 `refresh_a11y_bindings` 只重算 patch；帧本身按 a11y-only 结算
                    // （不重建 lane / 不编码 / 不产生 damage）。
                    let a11y_only =
                        target.tree().has_a11y_bind(node) && !target.tree().has_visual_bind(node);
                    let affects_layout = target
                        .tree()
                        .get(node)
                        .is_ok_and(|n| n.style_patch().affects_layout);
                    // `BIND` 是"重算动态属性"的唯一通道（R03-9）；`PAINT`/`LAYOUT`/`A11Y`
                    // 是它可能引发的下游失效，三者按语义各自独立登记。
                    let alive = if a11y_only {
                        let queued = target.a11y_note_binding(node);
                        let marked = target.tree_mut().mark_a11y(node);
                        queued || marked
                    } else if affects_layout {
                        target.tree_mut().mark_bind(node);
                        target.tree_mut().mark_layout(node)
                    } else {
                        target.tree_mut().mark_bind(node);
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
        // 批处理内只登记推迟位（最外层 `App::batch` 结束时合并成一次请求）。
        app.request_frame();
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{App, refresh_styles};
    use crate::{
        node::builder::NodeBuilder,
        text::{InlineAlign, InlineBoxSpec, TextRun, TextStyle},
        window::WindowOptions,
    };

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(runtime.handle().clone());
        (runtime, app)
    }

    /// v2.1 E2（`§11.2`）：`bind_text` 只换整段文本，**保留** runs（静态载荷不被清空）。
    #[test]
    fn bind_text_preserves_runs() {
        use crate::paint::color::Color;

        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(SmolStr::new_static("abcdef"));
        let bold = TextStyle::new(16.0).with_weight(700);
        let red = TextStyle::new(16.0).with_color(Color::WHITE);
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("abcdef", TextStyle::new(16.0))
                .runs([
                    TextRun::new(0..2, bold.clone()),
                    TextRun::new(2..6, red.clone()),
                ])
                .bind_text(move |cx| cx.read(&signal).clone())
                .build(w.tree_mut(), root)
                .expect("build")
        };
        let _ = refresh_styles(&mut app, window);
        let content = app
            .window(window)
            .expect("window")
            .tree()
            .text_content(node)
            .expect("text")
            .clone();
        assert_eq!(content.runs().len(), 2, "runs 在 bind_text 后必须保留");
        assert_eq!(content.runs()[0].range, 0..2);
        assert_eq!(content.runs()[0].style, bold);
        assert_eq!(content.runs()[1].range, 2..6);
        assert_eq!(content.runs()[1].style, red);
    }

    /// v2.1 E2：新文本更短时按新长度裁剪 runs（不越界），并压 `TextRunClipped` 诊断
    /// （不静默改变样式归属）。
    #[test]
    fn bind_text_crops_runs_to_the_new_text_and_diagnoses() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(SmolStr::new_static("abcdef"));
        let bold = TextStyle::new(16.0).with_weight(700);
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("abcdef", TextStyle::new(16.0))
                .runs([TextRun::new(0..2, bold.clone()), TextRun::new(2..6, bold)])
                .bind_text(move |cx| cx.read(&signal).clone())
                .build(w.tree_mut(), root)
                .expect("build")
        };
        let _ = refresh_styles(&mut app, window);
        app.write(&signal, SmolStr::new_static("abc"));
        let _ = refresh_styles(&mut app, window);
        let content = app
            .window(window)
            .expect("window")
            .tree()
            .text_content(node)
            .expect("text")
            .clone();
        assert_eq!(content.text, "abc");
        assert_eq!(content.runs().len(), 2, "裁剪保留 run（只内收区间）");
        assert_eq!(content.runs()[1].range, 2..3, "越界端点按新文本内收");
        assert!(
            app.text_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::text::TextDiagnostic::TextRunClipped { runs } if *runs == 1
            )),
            "裁剪必须可观测：{:?}",
            app.text_diagnostics()
        );
    }

    /// v2.1 E2：inline_boxes 与 runs 在同一段文本上共存（静态载荷不互相覆盖）。
    #[test]
    fn bind_text_preserves_runs_and_inline_boxes_together() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(SmolStr::new_static("a b"));
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::Size::new(8.0, 8.0),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("a b", TextStyle::new(16.0))
                .runs([TextRun::new(0..1, TextStyle::new(16.0).with_weight(700))])
                .inline_boxes([spec])
                .bind_text(move |cx| cx.read(&signal).clone())
                .build(w.tree_mut(), root)
                .expect("build")
        };
        let _ = refresh_styles(&mut app, window);
        let content = app
            .window(window)
            .expect("window")
            .tree()
            .text_content(node)
            .expect("text")
            .clone();
        assert_eq!(content.text, "a b");
        assert_eq!(content.runs().len(), 1);
        assert_eq!(content.inline_boxes.len(), 1);
    }
}

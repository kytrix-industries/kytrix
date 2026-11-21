//! 窗口集合：建 / 关 / 关闭前处理器、窗口控制请求、节点移除。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::App;
use crate::{
    error::EngineError,
    interaction,
    node::{dependency::Owner, id::NodeId},
    window::{Window, WindowId, WindowOptions},
};

/// 消费者：`App::remove_node`。
///
/// 删除浮层时顺带处理**焦点恢复**（R02-12）：`App::close_overlay`、`Dismiss` 动作与
/// 「点击外部关闭」三条路径都经过这里，把恢复集中在此才不会漏掉其中一条。
///
/// 删除子树时还必须**作废落在子树内的指针会话**（R02-8）：捕获目标被卸载后，旧实现让
/// `pointer_captured()` 永久返回死节点、平台捕获永不释放、按下记录卡在「按住」。
pub(crate) fn remove_node(app: &mut App, window: WindowId, node: NodeId) -> bool {
    // 先取浮层的 opener（删除之后槽随节点一起消失）。
    let opener = app.windows.get(&window).and_then(|target| {
        target
            .tree()
            .overlay_slot(node)
            .filter(|slot| slot.restore_focus)
            .and_then(|slot| slot.opener)
    });
    // 捕获目标（含「被删节点是捕获目标的祖先」）⇒ 先收尾会话：结束拖拽（派发
    // `DragPhase::Cancel`）、清按压/待定拖拽、请求平台释放捕获、清悬停。
    let captured = app.windows.get(&window).is_some_and(|target| {
        let tree = target.tree();
        tree.get(node).is_ok()
            && target
                .pointer()
                .capture
                .is_some_and(|captured| interaction::overlay::is_in_subtree(tree, captured, node))
    });
    if captured {
        app.cancel_pointer_session(window);
        // 观察者必须看到会话被系统/引擎取消（与平台 `PointerCancel` 同一语义），否则应用
        // 侧的「捕获中」状态永远等不到结束信号。
        app.events.push(
            window,
            crate::event::Event::Input(crate::event::InputEvent::PointerCancel {
                timestamp: std::time::Instant::now(),
            }),
        );
    }
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    // 卸载子树必须同步解开依赖边（P1-2.5）：`NodeTree::remove` 把被删节点的 deps 交还，
    // 这里逐条 detach。否则死边会随「挂载/卸载」次数无界累积，而 `App::write` 是最高频
    // 热路径 —— 成本会从 O(活跃读者) 退化成 O(历史挂载过的所有读者)。
    let removed = match target.tree_mut().remove(node) {
        Ok(detached) => {
            for (node, deps) in detached {
                for signal in deps {
                    app.graph.detach(signal, Owner::Node { window, node });
                }
            }
            true
        }
        Err(_) => false,
    };
    if removed {
        // E8：`bind_key_scoped` 的 scope 若落在被卸载的子树内，绑定必须同批清除 —— 否则
        // 表项会一直留到关窗（视觉上「没反应」，实际是永不命中的泄漏）。只卸载后代的场景
        // 不受影响：祖先 scope 仍在树里，绑定保留。
        let tree = target.tree();
        app.scoped_bindings
            .retain(|scoped| scoped.window != window || tree.get(scoped.scope).is_ok());
        // E10：`after_scoped` 的节点作用域若落在被卸载的子树内，定时器同批取消（与 scoped 键
        // 绑定同一生命周期规则）；不取消的话到期回调会跑在「组件已经不存在」的状态上。
        super::timer::retain_live_scopes(app, window);
        app.request_frame();
        super::focus::restore_focus(app, window, opener);
    }
    removed
}

// ── 窗口 ──────────────────────────────────────────────────────────────────

/// 本窗口的输入是否被某个**打开中的模态窗口**拦截（X1 引擎层模态兜底）。
///
/// 判据：存在一个窗口 `m`，`m.options().modal == true` 且 `m.options().owner == Some(window)`。
/// 即「本窗口是模态窗口的 owner」⇒ 模态期间 owner 不接收输入，直到模态窗口关闭。
///
/// 只拦截**直接 owner**（不递归 owner 链、不牵连 owner 的其它 owned 窗口）：与 Windows
/// `EnableWindow(owner, FALSE)` 的作用面一致 —— 被禁用的是 owner 本身；owned 窗口在系统
/// 语义里不是 owner 的子窗口（不是 `WS_CHILD`），不会自动禁用。
///
/// 消费者：`app::resources::dispatch`（三端统一兜底）。Windows 与 macOS 都另有**原生**拦截
/// （`EnableWindow(owner, FALSE)` / `ignoresMouseEvents` + 禁用标准按钮，RB-07）：这条引擎
/// 过滤保留为兜底 —— 原生拦截被 AppKit/系统绕过、或平台侧降级为无 owner 时，输入语义仍然
/// 与「模态窗口打开期间 owner 不接收输入」一致。
pub(crate) fn is_input_blocked_by_modal(app: &App, window: WindowId) -> bool {
    app.windows
        .values()
        .any(|candidate| candidate.options().modal && candidate.options().owner == Some(window))
}

/// 建一个窗口（headless：只建树与空场景）。`WindowId` 从 **1** 起单调 +1、永不复用
/// （0 保留为非法句柄，见 `WindowId::from_raw`）。
///
/// 同时铸造该窗口的**信号作用域**（`App::window_signal` 的归属；关窗时自动释放）。
///
/// # X1：owner / modal 的合法性
///
/// - `owner` 必须**已存在**（同一 `App` 内先创建）。不存在/已关闭 ⇒ 按 `None` 处理并写诊断：
///   陈旧句柄不得静默绑到新窗口（`WindowId` 永不复用，这里只是防御调用方传错）。
/// - `modal` 必须有 owner；没有 owner 的模态请求降级为普通窗口并写诊断（不存在「无主模态」
///   的合理语义，也不该 panic —— 建窗是应用常规路径）。
///
/// 级联生命周期：owner 被关闭时 owned 窗口一并关闭（见 [`close_window`]），与 Windows 原生
/// owner 语义一致（不询问 owned 窗口的关闭处理器）。
///
/// # Panics
/// 窗口句柄空间用尽（已建过 `u32::MAX` 个窗口）：`WindowId` 是 u32 的**单调不复用**句柄，
/// 没有回绕语义 —— 回绕会与早已关闭的窗口 id 冲突。这是「句柄用尽」的显式行为（A5）。
/// 消费者：`App::create_window`。
pub(crate) fn create_window(app: &mut App, mut options: WindowOptions) -> WindowId {
    if let Some(owner) = options.owner
        && !app.windows.contains_key(&owner)
    {
        eprintln!(
            "kytrix: window owner {owner:?} does not exist; creating an independent top-level window"
        );
        options.owner = None;
    }
    if options.modal && options.owner.is_none() {
        eprintln!(
            "kytrix: a modal window needs an existing owner; creating a non-modal top-level window"
        );
        options.modal = false;
    }
    let id = WindowId::from_raw(app.next_window_id);
    app.next_window_id = app
        .next_window_id
        .checked_add(1)
        .expect("window handle space exhausted (u32::MAX windows created)");
    let scope = app.store.new_scope();
    app.window_scopes.insert(id, scope);
    let scale = options.scale;
    let mut window = Window::new(id, options);
    // R03-7：树绑定窗口（之后铸出的 NodeId 带窗口身份，跨窗口误用确定性失败）。
    window.tree_mut().bind_window(id);
    // B7：非整数 DPI 才开启布局像素吸附（见 `NodeTree::set_device_scale` 的取舍说明）。
    window
        .tree_mut()
        .set_device_scale((scale.factor() != 1.0).then_some(scale.factor()));
    app.windows.insert(id, window);
    id
}

/// 只读窗口。
/// 消费者：`App::window`。
#[must_use]
pub(crate) fn window(app: &App, id: WindowId) -> Option<&Window> {
    app.windows.get(&id)
}

/// 可写窗口（应用层建树的入口）。
/// 消费者：`App::window_mut`。
pub(crate) fn window_mut(app: &mut App, id: WindowId) -> Option<&mut Window> {
    app.windows.get_mut(&id)
}

/// 设置窗口的自绘标题栏拖动区（X2；坐标与平台语义见 `Window::set_drag_regions`）。
///
/// 值变化才唤醒平台泵：泵每轮把最新区域对账到原生窗口（Windows 的 `WM_NCHITTEST` 表 /
/// macOS 的 `GuiView` 区域 / Linux 的 CSD move 判定）。窗口不存在时是 no-op。
/// 消费者：`App::set_drag_regions`。
pub(crate) fn set_drag_regions(app: &mut App, id: WindowId, regions: &[crate::math::Rect]) {
    let Some(target) = app.windows.get_mut(&id) else {
        return;
    };
    if target.drag_regions() == regions {
        return;
    }
    target.set_drag_regions(regions);
    app.dispatcher.wake();
}

/// 注册窗口的「关闭前」处理器：返回 `false` 拦下关闭（P2-8）。
///
/// 平台在用户点 X / Cmd-W 时先调用它；应用可在此保存未提交状态、弹确认框。
/// 窗口不存在时**忽略**注册（否则会留下一个永远不会被调用、却持有应用闭包的条目）。
/// 消费者：`App::on_close`。
pub(crate) fn on_close(
    app: &mut App,
    id: WindowId,
    handler: impl FnMut(&mut App) -> bool + 'static,
) {
    if !app.windows.contains_key(&id) {
        return;
    }
    app.close_handlers.insert(id, Box::new(handler));
}

/// 关闭询问的裁决（两阶段关闭的第一阶段结果；AR3 macOS P1-2）。
///
/// [`can_close_window`] 只**询问**、不动窗口状态；[`request_close`] 与 [`request_close_all`]
/// 共用它，差别只在询问之后要不要真的关。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseVerdict {
    /// 允许关闭：窗口仍在，由调用方在第二阶段真正关闭它。
    Allowed,
    /// 应用拦下（关闭处理器返回 `false`，且窗口仍然存在）。
    Vetoed,
    /// 窗口已不存在（调用时就没有，或关闭处理器自己关掉了它）：没有可拦的东西。
    ///
    /// 与 `Vetoed` 分开的理由：批量关闭（退出握手）里「窗口已经没了」不是否决，把退出整个
    /// 取消是语义错误（复审 §2.3 的 P3）。
    Gone,
}

/// 只询问、不关闭：跑一次关闭处理器并返回裁决（两阶段关闭的第一阶段）。
///
/// 处理器用「取出 → 调用 → 放回」的方式调用（与 [`request_close`] 同款）：它需要 `&mut App`，
/// 不能在使用 `app.close_handlers` 的借用期内存活。放回的两条判据也相同：处理器里可能已经把
/// 窗口关了，也可能**重新注册**了新的处理器（不能拿旧闭包覆盖它）。
///
/// 消费者：[`request_close`]、[`request_close_all`]。
pub(crate) fn can_close_window(app: &mut App, id: WindowId) -> CloseVerdict {
    let Some(mut handler) = app.close_handlers.remove(&id) else {
        // 没有处理器：不会 veto。窗口在 ⇒ 可关；不在 ⇒ 无事可做。
        return if app.windows.contains_key(&id) {
            CloseVerdict::Allowed
        } else {
            CloseVerdict::Gone
        };
    };
    let allow = handler(app);
    if app.windows.contains_key(&id) && !app.close_handlers.contains_key(&id) {
        app.close_handlers.insert(id, handler);
    }
    // 处理器自己关掉了窗口：没有可拦的对象，按「已经关了」处理（与 `allow` 无关 ——
    // 窗口已经不在了，再否决退出只会让用户卡在一个关不掉的退出流程里）。
    if !app.windows.contains_key(&id) {
        return CloseVerdict::Gone;
    }
    if allow {
        CloseVerdict::Allowed
    } else {
        CloseVerdict::Vetoed
    }
}

/// 平台关闭路径：先问关闭前处理器，允许才关。返回 `true` = 窗口已关闭。
///
/// 处理器用「取出 → 调用 → 放回」的方式调用（与 a11y handler 同一做法）：它需要
/// `&mut App`，不能在使用 `self.close_handlers` 的借用期内存活。
/// 消费者：`App::request_close`。
pub(crate) fn request_close(app: &mut App, id: WindowId) -> bool {
    match can_close_window(app, id) {
        // 询问通过 ⇒ 窗口仍在 ⇒ `close_window` 一定返回 true。
        CloseVerdict::Allowed => app.close_window(id),
        // 既有语义保持不变：被拦下与「窗口本来就不在」都返回 false（单窗口路径的调用方据此
        // 判断「这次关闭没有发生」；批量路径用 [`CloseVerdict::Gone`] 区分两者）。
        CloseVerdict::Vetoed | CloseVerdict::Gone => false,
    }
}

/// 两阶段批量关闭（AR3 macOS P1-2）：**先询问全部窗口，全部同意后才统一关闭**。
///
/// 返回 `false` = 某个窗口 veto ⇒ **一个窗口都没有被关闭**，调用方必须回滚整个流程
/// （macOS 退出握手就此回复 AppKit「不终止」）；返回 `true` = 全部批准且都已关闭。
///
/// 两个阶段的边界是**副作用**：询问阶段只跑关闭处理器（应用可能在此保存状态 / 弹确认框），
/// 不动任何窗口状态。这正是「逐窗 `request_close`」不能用于退出握手的原因 —— 它在批准时立刻
/// 关窗，于是「A 同意、B veto」会留下已经关掉的 A，而用户被告知「退出已取消」。
///
/// 每个处理器在一次调用里恰好跑一次：批准后第二阶段直接 `close_window`，不再问一遍（否则
/// 会两次弹确认框）。窗口已不存在（[`CloseVerdict::Gone`]）不算 veto：没有可拦的东西。
///
/// 只在 macOS 与测试构建里编译（唯一消费者是 macOS 的退出握手；其它平台一次只处理一个窗口
/// 的关闭请求）。
/// 消费者：`App::request_close_all`。
#[cfg(any(target_os = "macos", test))]
pub(crate) fn request_close_all(app: &mut App, ids: &[WindowId]) -> bool {
    let mut approved: Vec<WindowId> = Vec::with_capacity(ids.len());
    for &id in ids {
        match can_close_window(app, id) {
            CloseVerdict::Allowed => approved.push(id),
            // 任一窗口否决 ⇒ 整个握手回滚。询问阶段没有任何副作用，因此这里什么都不用撤销。
            CloseVerdict::Vetoed => return false,
            CloseVerdict::Gone => {}
        }
    }
    for id in approved {
        app.close_window(id);
    }
    true
}

/// 关窗：精确退订该窗口全部节点的依赖，然后丢弃。返回 `false` = 窗口不存在。
///
/// **X1：owner 级联** —— 关闭 owner 时先级联关闭它的 owned 窗口（递归，含 modal 窗口），
/// 与 Windows 原生 owner 语义一致：系统销毁 owner 会一并销毁 owned 窗口、且**不询问**
/// 它们的关闭处理器。因此级联是**无条件**的（应用主动 `close_window(owner)` 的语义就是
/// 「连同附属窗口一起关闭」；如果应用想逐窗征求否决，应自己先逐窗 `request_close`）。
///
/// 同时清理所有按窗口的旁表（关闭处理器 / a11y 限流戳 / scale epoch / 动画槽），
/// 否则 create/close 循环下这些表会无界增长。
/// 消费者：`App::close_window`。
pub(crate) fn close_window(app: &mut App, id: WindowId) -> bool {
    // 先级联 owned 窗口（递归由被调用的 `close_window` 完成）；先收集再递归，避免在
    // `app.windows` 的借用期内调用 `&mut App`。
    let owned: Vec<WindowId> = app
        .windows
        .iter()
        .filter(|(_, window)| window.options().owner == Some(id))
        .map(|(window, _)| *window)
        .collect();
    for child in owned {
        close_window(app, child);
    }
    // 按窗口的旁表必须一并清掉，否则 create/close 循环下无界增长 —— 尤其是
    // `close_handlers`：它持有应用闭包（可能捕获大量状态）。`WindowId` 单调不复用，
    // 所以不会串用，但泄漏是真实的（复审发现：早期提交声称清了，实际没清）。
    app.close_handlers.remove(&id);
    app.a11y_delivered.remove(&id);
    app.scale_epochs.remove(&id);
    app.image_scale_epochs.remove(&id);
    // E8：scoped 键盘绑定按窗口归属；窗口没了它们永远不会再命中（焦点只能是本窗口节点），
    // 必须在关窗时清掉，否则 create/close 循环下无界增长。
    app.scoped_bindings.retain(|scoped| scoped.window != id);
    // E1b：焦点窗口记录不能指向已关闭的窗口；`default_text_scale` 保留最后值
    //（「无窗口时保持上次值」）。
    if app.focused_window == Some(id) {
        app.focused_window = None;
    }
    // 平台 IME 目标状态：窗口没了就没有「已应用状态」可对齐，留着只会在 create/close
    // 循环下无界增长（WindowId 不复用，不会串到新窗口）。
    app.ime_disabled.remove(&id);
    // 动画槽引用窗口内的节点与瞬态值：窗口关闭时必须一并清掉（否则 `SetTimer` 永不停）。
    app.animations.retain(|slot| slot.window != id);
    // E10：该窗口的一次性定时器与尚未派发的动画终止事件一并丢弃 —— 窗口与节点已经不存在，
    // 应用回调没有可收尾的对象；留着只会在下一轮把死窗口句柄交出去。
    super::timer::cancel_window(app, id);
    super::animation::drop_completions_for_window(app, id);
    // 「按住集合」是每窗口状态（R02-16）：关掉持有键盘焦点的窗口后，它按住的键不得继续
    // 在 `key_down` 里为真。指针捕获请求与待推光标同理 —— 窗口没了，请求没有接收方，
    // 留着只会在 create/close 循环下无界增长。
    app.held_keys.remove(&id);
    if app.cursor_pending.is_some_and(|(window, _)| window == id) {
        app.cursor_pending = None;
    }
    // 窗口关闭 = 整个指针会话作废：先收尾（结束拖拽并派发 `DragPhase::Cancel`、清按压/悬停），
    // 再丢弃窗口。单节点的「捕获目标被卸载」由 `remove_node` 处理；关窗时平台捕获随
    // `DestroyWindow` 一起释放（Windows 语义），这里只需让应用看到会话取消。
    if app.windows.contains_key(&id) {
        app.cancel_pointer_session(id);
    }
    // 捕获请求表必须在 `cancel_pointer_session` **之后**清：收尾本身会写一条
    // `(window, false)` 请求（结束拖拽 / 释放捕获），先清再收尾会把它留在表里 —— 已关闭窗口
    // 的请求没有接收方（泵找不到窗口直接跳过），纯属白写一次（AR1b P3-6）。
    app.capture_requests.remove(&id);
    #[cfg(feature = "hud")]
    app.debug_hud.remove(&id);
    app.last_click = app.last_click.filter(|record| record.window != id);
    let Some(window) = app.windows.remove(&id) else {
        return false;
    };
    // 窗口没了，它名下（`insert_owned`）的图片引用也必须归还：tile 恒有效契约的对偶面。
    // 放在窗口移除**之后**，重复 close（窗口已不存在）会提前 return false，不会误释放。
    app.resources.release_window(id);
    // E7：释放的 tile 可能被**其它窗口**的图片节点引用（跨窗口共享句柄）。那些窗口的
    // paint lane 里还烘焙着旧 tile，必须当帧失效 —— 否则会把已回收（可能已被别的图复用）
    // 的图继续画出来。冷路径（关窗）做一次全窗扫描是可接受的。
    if super::resources::invalidate_dead_images(app) > 0 {
        app.request_frame();
    }
    // 关窗是冷路径：一次 DFS 收集句柄（不再有全序数组可借）。
    let nodes = window.tree().dfs_ids();
    for node in nodes {
        if let Some(deps) = window.tree().deps(node) {
            for signal in deps {
                app.graph.detach(*signal, Owner::Node { window: id, node });
            }
        }
    }
    // 窗口的信号作用域：释放后其信号的值**立即回收**（槽位归还空闲栈、代际递增），旧句柄的写
    // 是 no-op、读是确定性失败，依赖边被立即删除。
    // 这是「关窗之后的迟到写入（异步任务 / 定时器回投）不得产生僵尸唤醒」的可观测半边
    // （R03-6 / wave-6）：节点退订处理了**读侧**，作用域释放处理了**写侧**；S6 起写侧同时
    // 不再让槽位与其值常驻（长跑窗口 churn 的槽位峰值不增长）。
    if let Some(scope) = app.window_scopes.remove(&id) {
        super::signal_api::release_scope(app, scope);
    }
    true
}

/// 请求一次窗口控制（自绘标题栏的系统按钮 / 拖动 / 缩放；E6 起三端统一）。
///
/// 能力判定由平台层给出（[`crate::platform::native::unsupported_window_control`]）：该端没有
/// 等价请求时返回 [`EngineError::UnsupportedWindowCapability`]（I10，不静默排队后再丢弃）。
/// 其余请求登记进单槽，由平台泵在**同一轮 / 下一轮**执行（`ShowWindow` / `performZoom:` 等系统
/// 调用不能发生在持有 `&mut App` 的借用里）。
///
/// 消费者：`App::request_window_control`。
///
/// # Errors
/// - [`EngineError::UnknownWindow`]：窗口不存在（已经关闭；优先于能力错误）。
/// - [`EngineError::UnsupportedWindowCapability`]：该端没有这个控制。
pub(crate) fn request_window_control(
    app: &mut App,
    window: WindowId,
    control: crate::window::WindowControl,
) -> Result<(), EngineError> {
    if !app.windows.contains_key(&window) {
        return Err(EngineError::UnknownWindow { window });
    }
    if let Some(capability) = crate::platform::native::unsupported_window_control(control) {
        return Err(EngineError::UnsupportedWindowCapability { window, capability });
    }
    // 移动 / 缩放一旦交给 compositor，客户端就收不到后续指针抬起：主动结束本地拖拽会话，
    // 避免下一次移动被误当成标题栏/边框拖拽（Wayland 专属路径；Windows/macOS 的
    // `BeginMove` / `BeginResize` 已在上面按能力拒绝）。
    #[cfg(target_os = "linux")]
    if matches!(
        control,
        crate::window::WindowControl::BeginMove | crate::window::WindowControl::BeginResize(_)
    ) {
        end_local_drag_session(app, window);
    }
    app.window_control = Some((window, control));
    Ok(())
}

/// Linux：把本地指针拖拽会话收尾（`xdg_toplevel.move` / `resize` 之后不会再收到 Up）。
#[cfg(target_os = "linux")]
fn end_local_drag_session(app: &mut App, window: WindowId) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    target.pointer_mut().end_drag();
    target.pointer_mut().press = None;
    app.capture_requests.insert(window, false);
}

/// 取走待执行的窗口控制请求（三端泵的唯一读点）。
/// 消费者：三端事件循环（`Take` 之后在不持有 `&mut App` 的借用里执行系统调用）。
pub(crate) fn take_window_control(
    app: &mut App,
) -> Option<(WindowId, crate::window::WindowControl)> {
    app.window_control.take()
}

/// 运行时改标题（写窗口参数 + 唤醒泵；平台泵每轮对账到原生窗口）。
///
/// 同值 no-op 在这里短路：避免每轮泵都做一次 `SetWindowTextW` / `setTitle:`（应用常在
/// `on_frame` 里按行情更新标题，值没变时不该有系统调用）。
///
/// # Errors
/// [`EngineError::UnknownWindow`]：窗口不存在（已经关闭）。
pub(crate) fn set_title(
    app: &mut App,
    window: WindowId,
    title: smol_str::SmolStr,
) -> Result<(), EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    if target.options().title == title {
        return Ok(());
    }
    target.set_title(title);
    app.dispatcher.wake();
    Ok(())
}

/// macOS：写交通灯目标位置（**set-once**：只在建窗时决定一次）并唤醒泵。
///
/// 首次调用之后再次设置不改变已登记的位置（返回 `Ok(())` 且不唤醒泵）：引擎不保存 AppKit
/// 系统按钮布局的副本，运行期改位置 / 改回 `None` 都不可撤销（§8.6.5 的文档化限制）。
#[cfg(target_os = "macos")]
pub(crate) fn set_traffic_light_position(
    app: &mut App,
    window: WindowId,
    inset: crate::math::Point,
) -> Result<(), EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    // set-once：已有目标位置 ⇒ 不覆盖、不唤醒（运行期修改被忽略，见函数文档）。
    if target.traffic_light_position().is_some() {
        return Ok(());
    }
    target.set_traffic_light_position(inset);
    app.dispatcher.wake();
    Ok(())
}

/// 非 macOS：交通灯位置没有等价能力 ⇒ 强类型错误（I10；调用方按变体处理，不静默）。
#[cfg(not(target_os = "macos"))]
pub(crate) fn set_traffic_light_position(
    app: &mut App,
    window: WindowId,
    _inset: crate::math::Point,
) -> Result<(), EngineError> {
    if !app.windows.contains_key(&window) {
        return Err(EngineError::UnknownWindow { window });
    }
    Err(EngineError::UnsupportedWindowCapability {
        window,
        capability: "traffic light position",
    })
}

/// macOS：交通灯安全区（泵写入的只读镜像；窗口不存在 ⇒ `None`）。
#[cfg(target_os = "macos")]
pub(crate) fn traffic_light_inset(app: &App, window: WindowId) -> Option<crate::math::Point> {
    app.windows.get(&window)?.traffic_light_inset()
}

/// 非 macOS：没有交通灯 ⇒ `None`（只读查询的契约，不是错误）。
#[cfg(not(target_os = "macos"))]
pub(crate) fn traffic_light_inset(_app: &App, _window: WindowId) -> Option<crate::math::Point> {
    None
}

/// 遍历全部窗口（顺序不保证）并带出 `WindowId`。
///
/// 带上 id 是必要的：帧是**按窗口**推进的（`refresh_styles(window)` / P06 的 paint），只有 `&Window`
/// 拿不到「这是哪个窗口」（消费者：P09 的帧驱动、测试）。
/// 消费者：`App::windows`。
pub(crate) fn windows(app: &App) -> impl Iterator<Item = (WindowId, &Window)> {
    app.windows.iter().map(|(id, window)| (*id, window))
}

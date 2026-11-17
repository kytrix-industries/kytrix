//! 窗口集合：建 / 关 / 关闭前处理器、窗口控制请求、节点移除。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::App;
#[cfg(target_os = "linux")]
use crate::error::EngineError;
use crate::{
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
        app.request_frame();
        super::focus::restore_focus(app, window, opener);
    }
    removed
}

// ── 窗口 ──────────────────────────────────────────────────────────────────

/// 建一个窗口（headless：只建树与空场景）。`WindowId` 从 **1** 起单调 +1、永不复用
/// （0 保留为非法句柄，见 `WindowId::from_raw`）。
/// 消费者：`App::create_window`。
pub(crate) fn create_window(app: &mut App, options: WindowOptions) -> WindowId {
    let id = WindowId::from_raw(app.next_window_id);
    app.next_window_id = app.next_window_id.wrapping_add(1);
    app.windows.insert(id, Window::new(id, options));
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

/// 平台关闭路径：先问关闭前处理器，允许才关。返回 `true` = 窗口已关闭。
///
/// 处理器用「取出 → 调用 → 放回」的方式调用（与 a11y handler 同一做法）：它需要
/// `&mut App`，不能在使用 `self.close_handlers` 的借用期内存活。
/// 消费者：`App::request_close`。
pub(crate) fn request_close(app: &mut App, id: WindowId) -> bool {
    let Some(mut handler) = app.close_handlers.remove(&id) else {
        return app.close_window(id);
    };
    let allow = handler(app);
    // 放回的两条判据：
    // - 处理器里可能已经把窗口关了（自己调用 `close_window`）⇒ 不要再放回；
    // - 处理器里可能**重新注册**了新的处理器 ⇒ 不能拿旧闭包覆盖它。
    if app.windows.contains_key(&id) && !app.close_handlers.contains_key(&id) {
        app.close_handlers.insert(id, handler);
    }
    if !allow {
        return false;
    }
    app.close_handlers.remove(&id);
    // 处理器可能**自己**已经关掉了窗口：`close_window` 返回 false 但「窗口已关闭」
    // 正是 `true` 的语义，不能把它当「被拦下」（复审 F8）。
    let existed = app.windows.contains_key(&id);
    app.close_window(id);
    // 自己关掉（原本存在、现在没了）也算关闭成功。
    existed
}

/// 关窗：精确退订该窗口全部节点的依赖，然后丢弃。返回 `false` = 窗口不存在。
///
/// 同时清理所有按窗口的旁表（关闭处理器 / a11y 限流戳 / scale epoch / 动画槽），
/// 否则 create/close 循环下这些表会无界增长。
/// 消费者：`App::close_window`。
pub(crate) fn close_window(app: &mut App, id: WindowId) -> bool {
    // 按窗口的旁表必须一并清掉，否则 create/close 循环下无界增长 —— 尤其是
    // `close_handlers`：它持有应用闭包（可能捕获大量状态）。`WindowId` 单调不复用，
    // 所以不会串用，但泄漏是真实的（复审发现：早期提交声称清了，实际没清）。
    app.close_handlers.remove(&id);
    app.a11y_delivered.remove(&id);
    app.scale_epochs.remove(&id);
    // 平台 IME 目标状态：窗口没了就没有「已应用状态」可对齐，留着只会在 create/close
    // 循环下无界增长（WindowId 不复用，不会串到新窗口）。
    app.ime_disabled.remove(&id);
    // 动画槽引用窗口内的节点与瞬态值：窗口关闭时必须一并清掉（否则 `SetTimer` 永不停）。
    app.animations.retain(|slot| slot.window != id);
    // 「按住集合」是每窗口状态（R02-16）：关掉持有键盘焦点的窗口后，它按住的键不得继续
    // 在 `key_down` 里为真。指针捕获请求与待推光标同理 —— 窗口没了，请求没有接收方，
    // 留着只会在 create/close 循环下无界增长。
    app.held_keys.remove(&id);
    app.capture_requests.remove(&id);
    if app.cursor_pending.is_some_and(|(window, _)| window == id) {
        app.cursor_pending = None;
    }
    // 窗口关闭 = 整个指针会话作废：先收尾（结束拖拽并派发 `DragPhase::Cancel`、清按压/悬停），
    // 再丢弃窗口。单节点的「捕获目标被卸载」由 `remove_node` 处理；关窗时平台捕获随
    // `DestroyWindow` 一起释放（Windows 语义），这里只需让应用看到会话取消。
    if app.windows.contains_key(&id) {
        app.cancel_pointer_session(id);
    }
    #[cfg(feature = "hud")]
    app.debug_hud.remove(&id);
    app.last_click = app.last_click.filter(|record| record.window != id);
    let Some(window) = app.windows.remove(&id) else {
        return false;
    };
    // 关窗是冷路径：一次 DFS 收集句柄（不再有全序数组可借）。
    let nodes = window.tree().dfs_ids();
    for node in nodes {
        if let Some(deps) = window.tree().deps(node) {
            for signal in deps {
                app.graph.detach(*signal, Owner::Node { window: id, node });
            }
        }
    }
    true
}

/// 请求一次 Linux 客户端装饰的窗口控制（自绘标题栏专用）。
///
/// 请求由 Wayland 事件循环在**同一轮**内执行；`BeginMove` / `BeginResize` 使用最近一次
/// 指针按键的 serial，因此只应从输入事件的动作处理器里调用（拖动/点击的 handler）。
/// 其它平台有原生标题栏，不提供本入口。
///
/// # Errors
/// `EngineError::UnknownWindow`：窗口不存在（已经关闭）。
/// 消费者：`App::request_window_control`。
#[cfg(target_os = "linux")]
pub(crate) fn request_window_control(
    app: &mut App,
    window: WindowId,
    control: crate::window::WindowControl,
) -> Result<(), EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    if matches!(
        control,
        crate::window::WindowControl::BeginMove | crate::window::WindowControl::BeginResize(_)
    ) {
        // 移动/缩放一旦交给 compositor，客户端就收不到后续指针抬起：主动结束本地拖拽
        // 会话，避免下一次移动被误当成标题栏/边框拖拽。
        target.pointer_mut().end_drag();
        target.pointer_mut().press = None;
        app.capture_requests.insert(window, false);
    }
    app.window_control = Some((window, control));
    Ok(())
}

/// 取走待执行的窗口控制请求（Wayland 事件循环的唯一读点）。
/// 消费者：`App::take_window_control`。
#[cfg(target_os = "linux")]
pub(crate) fn take_window_control(
    app: &mut App,
) -> Option<(WindowId, crate::window::WindowControl)> {
    app.window_control.take()
}

/// 遍历全部窗口（顺序不保证）并带出 `WindowId`。
///
/// 带上 id 是必要的：帧是**按窗口**推进的（`refresh_styles(window)` / P06 的 paint），只有 `&Window`
/// 拿不到「这是哪个窗口」（消费者：P09 的帧驱动、测试）。
/// 消费者：`App::windows`。
pub(crate) fn windows(app: &App) -> impl Iterator<Item = (WindowId, &Window)> {
    app.windows.iter().map(|(id, window)| (*id, window))
}

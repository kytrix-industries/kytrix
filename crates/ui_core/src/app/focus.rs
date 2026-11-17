//! 焦点与 IME：焦点权威源读写、Tab 序、焦点作用域、悬停读点、IME caret hint 与每窗口 IME 目标状态。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::App;
use crate::{
    error::EngineError,
    interaction::{focus, overlay as overlay_util, scroll},
    math::Rect,
    node::id::NodeId,
    window::{Window, WindowId},
};

// ── P10：焦点 ──────────────────────────────────────────────────────────────

/// 当前焦点（`None` = 无焦点 / 窗口不存在 / **焦点节点已卸载**）。
///
/// 陈旧焦点一律视为无焦点：虚拟列表回收行之后，旧 `NodeId` 不得再驱动键盘路由。
/// 消费者：`App::focused`。
#[must_use]
pub(crate) fn focused(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.focus().focused()?;
    target.tree().get(node).ok().map(|_| node)
}

/// 当前的焦点作用域根（最上层模态浮层的子树根；R02-12）。
/// 消费者：`App::focus_scope`。
#[must_use]
pub(crate) fn focus_scope(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let tree = target.tree();
    overlay_util::focus_scope_root(tree, tree.root())
}

/// 指针当前悬停的节点（陈旧节点视为无悬停）。
/// 消费者：`App::hovered`。
#[must_use]
pub(crate) fn hovered(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.pointer().hovered?;
    target.tree().get(node).ok().map(|_| node)
}

/// 清掉指向已卸载节点的焦点（读取路径都是只读，清理集中在这里）。
/// 消费者：`App::validate_focus`。
pub(crate) fn validate_focus(app: &mut App, window: WindowId) {
    let stale = app.windows.get(&window).and_then(|target| {
        let node = target.focus().focused()?;
        target.tree().get(node).is_err().then_some(())
    });
    if stale.is_some()
        && let Some(target) = app.windows.get_mut(&window)
    {
        target.focus_mut().set(None);
    }
}

/// 设置焦点。不可聚焦 / 同值 / 作用域外 ⇒ `false`（不标脏、不唤醒）。
/// 消费者：`App::set_focus`。
pub(crate) fn set_focus(app: &mut App, window: WindowId, node: Option<NodeId>) -> bool {
    let scope = focus_scope(app, window);
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    if let Some(node) = node
        && !focus::focusable_in_scope(target.tree(), node, scope)
    {
        return false;
    }
    let old = target.focus().focused();
    if !target.focus_mut().set(node) {
        return false;
    }
    // 旧焦点环超出节点 bounds：清环时把环的外扩并入 damage（否则边缘残留 1px）。
    if let Some(previous) = old
        && let Some(ring) = target.tree().focus_ring(previous)
        && let Ok(slot) = target.tree().get(previous)
    {
        let bounds = slot.layout().bounds;
        let screen =
            scroll::to_screen(target.tree(), previous, bounds).outset(ring.offset + ring.width);
        target.request_damage(screen);
    }
    for changed in [old, node].into_iter().flatten() {
        let has_ring = target.tree().focus_ring(changed).is_some();
        target.tree_mut().mark_a11y(changed);
        if has_ring {
            target.tree_mut().mark_paint(changed);
        }
    }
    app.request_frame();
    true
}

/// 关闭浮层后把焦点还给**打开者**（R02-12）；返回是否真的改了焦点。
///
/// `opener` 已被卸载（带动画的对话框把触发按钮一起拆了）或已不可聚焦时保持现状 ——
/// 静默恢复到「无焦点」比指到一个不存在的节点更糟（a11y 焦点会漂移）。
/// 消费者：`App::close_overlay` / `App::dismiss_overlay`。
pub(crate) fn restore_focus(app: &mut App, window: WindowId, opener: Option<NodeId>) -> bool {
    let Some(opener) = opener else {
        return false;
    };
    let alive = app
        .windows
        .get(&window)
        .is_some_and(|target| target.tree().get(opener).is_ok());
    if !alive {
        return false;
    }
    app.set_focus(window, Some(opener))
}

/// 设置 IME caret hint（P16）：`node` 是该窗口内的文本节点，`byte_index` 是 UTF-8 字节偏移。
///
/// 允许在文本尚未 shape 时调用；**`byte_index` 的越界/char-boundary 校验在读取时做**，
/// 因为信号写入到下一次 text pass 之间文本内容可能还是旧快照。该调用**不标脏、不请求重绘**，
/// 只在 hint 变化时唤醒平台事件循环。
///
/// # Errors
/// `UnknownWindow`：窗口不存在；`StaleNode`：节点不存在；`TextShape`：不是文本节点。
/// 消费者：`App::set_ime_cursor_area`。
pub(crate) fn set_ime_cursor_area(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    byte_index: usize,
) -> Result<(), EngineError> {
    let changed = {
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        let Some(slot) = target.tree().get(node).ok() else {
            return Err(EngineError::StaleNode { node });
        };
        if slot.kind().text().is_none() {
            return Err(EngineError::TextShape {
                node,
                detail: smol_str::SmolStr::new_static("node is not a text node"),
            });
        }
        target.set_ime_cursor_request(node, byte_index)
    };
    if changed {
        app.dispatcher.wake();
    }
    Ok(())
}

/// 清除 IME caret hint（P16）；不存在的窗口是 no-op。
/// 消费者：`App::clear_ime_cursor_area`。
pub(crate) fn clear_ime_cursor_area(app: &mut App, window: WindowId) {
    let changed = app
        .windows
        .get_mut(&window)
        .is_some_and(Window::clear_ime_cursor_request);
    if changed {
        app.dispatcher.wake();
    }
}

/// 启用/禁用某个窗口的输入法（IME，P2-7）。
///
/// 禁用后该窗口不再接收 IME 组合输入：Windows 平台在下一轮泵里对窗口调用
/// `ImmAssociateContext(hwnd, None)` 把输入上下文摘下来（旧 `HIMC` 由平台保存），字母/
/// 数字键因此作为普通 `WM_KEYDOWN` 到达应用；重新启用时恢复保存的上下文（禁用时窗口
/// 本来就没有上下文则让系统关联默认上下文）。窗口销毁前平台会自动恢复关联，`HIMC`
/// 不会泄漏。
///
/// 典型用途：自绘搜索框的「英文直输」模式、只接受快捷键的图表/表格区 —— 用户开着
/// 中文/日文输入法时，字母键不会被 IME 变成组合输入。
///
/// 这是**每窗口**的目标状态，默认启用；状态变化不标脏、不请求重绘，只在变化时唤醒
/// 平台事件循环。窗口不存在时是 no-op。
///
/// 若调用时该窗口正在 IME 组合中，禁用会直接丢弃组合（不补发 `ImeEvent::End`）：
/// 调用方应在关闭 IME 的同时自己清空 preedit 状态。
///
/// 平台支持：目前只有 Windows 落实（macOS / Linux 保持系统默认 IME 行为）。
/// 消费者：`App::set_ime_enabled`。
pub(crate) fn set_ime_enabled(app: &mut App, window: WindowId, enabled: bool) {
    if !app.windows.contains_key(&window) {
        return;
    }
    let changed = if enabled {
        app.ime_disabled.remove(&window)
    } else {
        app.ime_disabled.insert(window)
    };
    if changed {
        app.dispatcher.wake();
    }
}

/// 当前 IME caret hint 对应的窗口客户区逻辑矩形（P16）。
///
/// `Ok(None)` = 没有设置 hint；`Err(TextShape)` = 已设置但文本尚未 shape / 文本变化，
/// 平台层应跳过本次更新而不是清除上一次有效位置。
///
/// # Errors
/// `UnknownWindow`：窗口不存在；`StaleNode` / `TextShape`：hint 指向的文本节点不可解析。
/// 消费者：`App::ime_cursor_area`。
pub(crate) fn ime_cursor_area(app: &App, window: WindowId) -> Result<Option<Rect>, EngineError> {
    app.windows
        .get(&window)
        .ok_or(EngineError::UnknownWindow { window })?
        .ime_cursor_area()
}

/// 焦点移到下一个可聚焦节点（末尾回绕；模态浮层内循环，R02-12）。
/// 消费者：`App::focus_next`。
pub(crate) fn focus_next(app: &mut App, window: WindowId) -> Option<NodeId> {
    app.validate_focus(window);
    let scope = focus_scope(app, window);
    let next = app.windows.get(&window).and_then(|target| {
        focus::next_focusable(target.tree(), target.focus().focused(), false, scope)
    });
    if app.set_focus(window, next) {
        next
    } else {
        None
    }
}

/// 焦点移到上一个可聚焦节点（开头回绕；模态浮层内循环，R02-12）。
/// 消费者：`App::focus_prev`。
pub(crate) fn focus_prev(app: &mut App, window: WindowId) -> Option<NodeId> {
    app.validate_focus(window);
    let scope = focus_scope(app, window);
    let next = app.windows.get(&window).and_then(|target| {
        focus::next_focusable(target.tree(), target.focus().focused(), true, scope)
    });
    if app.set_focus(window, next) {
        next
    } else {
        None
    }
}

/// 本窗口的 IME 目标状态（Windows 平台泵每轮读它与已应用状态比较，P2-7）。
///
/// 只在 Windows 平台层消费，因此按平台门控：其它平台的 `set_ime_enabled` 只记录状态，
/// 没有读取方（也就不会出现 dead_code 警告）。
/// 消费者：`App::ime_enabled`。
#[cfg(target_os = "windows")]
pub(crate) fn ime_enabled(app: &App, window: WindowId) -> bool {
    !app.ime_disabled.contains(&window)
}

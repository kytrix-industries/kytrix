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

/// 当前焦点（`None` = 无焦点 / 窗口不存在 / **焦点节点已卸载** / **焦点在模态作用域外**）。
///
/// 陈旧焦点一律视为无焦点：虚拟列表回收行之后，旧 `NodeId` 不得再驱动键盘路由。
/// 模态浮层（`trap_focus`）打开后，落在作用域外的焦点同样视为无焦点 —— 背景控件不得再被
/// Enter/Space 或 a11y 激活；下一次输入会把它移入作用域内（[`validate_focus`]）。
/// **完全没有焦点**时同理：模态作用域存在的话，下一次输入会把作用域内第一个可聚焦节点填上
/// （AR1b N1b：从不可聚焦内容打开对话框后 Enter 必须有落点）。
/// 消费者：`App::focused`。
#[must_use]
pub(crate) fn focused(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.focus().focused()?;
    let tree = target.tree();
    if tree.get(node).is_err() {
        return None;
    }
    let scope = overlay_util::focus_scope_root(tree, tree.root());
    focus::in_scope(tree, node, scope).then_some(node)
}

/// 当前的焦点作用域根（最上层模态浮层的子树根；R02-12）。
/// 消费者：`App::focus_scope`。
#[must_use]
pub(crate) fn focus_scope(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let tree = target.tree();
    overlay_util::focus_scope_root(tree, tree.root())
}

/// 指针当前悬停的节点（陈旧节点 / **模态作用域外** ⇒ 无悬停）。
/// 消费者：`App::hovered`。
#[must_use]
pub(crate) fn hovered(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.pointer().hovered?;
    let tree = target.tree();
    if tree.get(node).is_err() {
        return None;
    }
    // 模态作用域（E12）：作用域外的悬停读作 `None` —— tooltip / `:hover` 的驱动方（应用层）
    // 由此天然拿不到背景节点，不必各自重实现「模态挡住了吗」。
    let scope = overlay_util::focus_scope_root(tree, tree.root());
    overlay_util::in_modal_scope(tree, node, scope).then_some(node)
}

/// 清理失效焦点（读取路径都是只读，清理集中在这里）。
///
/// 三条规则（R02-12 / AR1b N1b）：
/// 1. 指向**已卸载节点**的焦点 ⇒ 清空；
/// 2. **完全没有焦点**且存在模态作用域 ⇒ 移入作用域内第一个可聚焦节点；
/// 3. 落在**模态作用域外**的焦点 ⇒ 移入作用域内第一个可聚焦节点（没有候选则清空）。
///
/// 规则 2/3 是「模态打开时把焦点移入对话框」的落点：浮层的子节点在 `create_overlay` 返回之后
/// 才挂载，创建瞬间求值不到候选，因此修复放在下一次输入/焦点操作（本函数是它们的公共入口）。
/// 规则 2 覆盖「从不可聚焦内容（图表区 / 自绘画布）打开对话框」：此时焦点本来就是 `None`，
/// 不修复的话 Enter/Space 没有落点，用户必须先按一次 Tab 或点一下（AR1b 探针 7）。
/// 常见路径（没有模态）只做一次根子节点扫描，零分配。
/// 消费者：`App::validate_focus`。
pub(crate) fn validate_focus(app: &mut App, window: WindowId) {
    let repair = {
        let Some(target) = app.windows.get(&window) else {
            return;
        };
        let tree = target.tree();
        let scope = overlay_util::focus_scope_root(tree, tree.root());
        let first_in_scope =
            || scope.and_then(|scope| focus::next_focusable(tree, None, false, Some(scope)));
        match target.focus().focused() {
            // 从不可聚焦内容打开模态：焦点从未存在，必须补一个落点（否则 Enter 无处可去）。
            None => first_in_scope().map(Some),
            Some(node) if tree.get(node).is_err() => Some(None),
            Some(node) if scope.is_some() && !focus::in_scope(tree, node, scope) => {
                first_in_scope().map(Some)
            }
            Some(_) => None,
        }
    };
    match repair {
        // 陈旧焦点：只清位（节点已卸载，没有可标脏/发 a11y 的对象）。
        Some(None) => {
            if let Some(target) = app.windows.get_mut(&window) {
                target.focus_mut().set(None);
            }
        }
        // 作用域外焦点 / 无焦点：走 `set_focus` 的完整路径（焦点环 damage + a11y 标记 + 唤醒）。
        Some(Some(next)) => {
            app.set_focus(window, Some(next));
        }
        None => {}
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

/// 启用/禁用某个窗口的输入法（IME，P2-7；三端统一语义 B6/B9）。
///
/// 禁用后该窗口的按键**不再路由给 IME**，字母/数字作为普通 `InputEvent::Key` /
/// `InputEvent::Text` 到达应用；重新启用恢复系统路由：
///
/// - **Windows**（已有路径，本次对齐文档）：下一轮泵对窗口调用 `ImmAssociateContext(hwnd,
///   None)` 摘上下文（旧 `HIMC` 由平台保存），重新启用时恢复；窗口销毁前平台自动恢复关联。
/// - **macOS**：`GuiView` 的 `inputContext` 返回 `nil`（WebKit `-[WKView inputContext]` 的
///   「非 editable 返回 nil」同款做法），按键因此走普通 `keyDown:` → `characters` 通路；
///   禁用瞬间对当前上下文 `discardMarkedText` 并清空组合状态。
/// - **Linux**：焦点目标的 `zwp_text_input_v3.disable()`（协议要求「没有输入焦点时
///   disable」），重新启用且目标仍是文本时整份重发 `enable` + `set_*` 状态。
///
/// 这是**每窗口的目标状态**，默认启用；状态变化不标脏、不请求重绘，只在变化时唤醒
/// 平台事件循环。窗口不存在时是 no-op。
///
/// 若调用时该窗口正在 IME 组合中，禁用会丢弃组合；丢弃时**补发一条
/// [`ImeEvent::End`]（与焦点移走/窗口失焦的统一清场路径一致：三端都先清 caret hint、
/// 重置组合状态，再补 `End`），应用不会停在 preedit 状态。**不**改变引擎焦点。
///
/// 典型用途：自绘搜索框的「英文直输」模式、只接受快捷键的图表/表格区 —— 用户开着
/// 中文/日文输入法时，字母键不会被 IME 变成组合输入。
///
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

/// 本窗口的 IME 目标状态（三端平台泵每轮读它与已应用状态比较，P2-7 / B6 / B9）。
///
/// 消费者（三端）：`platform::windows::window::NativeWindow::sync_ime_enabled`、
/// `platform::macos::window::NativeWindow::sync_ime_enabled`、
/// `platform::linux` 的每轮 `sync_ime_policy`。
pub(crate) fn ime_enabled(app: &App, window: WindowId) -> bool {
    !app.ime_disabled.contains(&window)
}

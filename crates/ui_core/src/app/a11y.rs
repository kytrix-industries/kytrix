//! 无障碍：动作 handler 表、诊断环、限流交付与快照捕获。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::{A11Y_DIAGNOSTIC_CAPACITY, A11Y_THROTTLE, App};
use crate::window::{Window, WindowId};

// ── P11：无障碍（动作 / 诊断 / 交付） ──────────────────────────────────

/// 注册 a11y 动作的处理器（同 `A11yAction` 后写覆盖）。
///
/// 返回 `false` = 该动作**不能**注册：`A11yAction::Click` 必须落到 P10 的 `Action` 表
/// （`App::on_action`），让指针点击 / 键盘 Enter / AT 激活共享同一身份
/// （`design/phase-11-a11y.md` §9.3）。
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
#[must_use]
pub(crate) fn a11y_pending(app: &App, window: WindowId) -> bool {
    let dirty = app.windows.get(&window).is_some_and(Window::a11y_pending);
    dirty
        && app
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

/// 产出一笔 a11y 交付（消费 `A11Y|ORDER` 位与 `full_pending`）。
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

//! P11 a11y：快照交付与捕获（窗口侧入口）。

use super::Window;

// 消费者：`impl Window::a11y_pending` 的薄委托（外部调用点见该方法文档）。
// ── P11：a11y（快照交付 + 捕获） ─────────────────────────────────────────

/// 本轮是否有 a11y 工作（O(1)；平台泵每轮用它过滤 `update_if_active` 调用）。
pub(crate) fn a11y_pending(window: &Window) -> bool {
    window.a11y.pending(&window.tree)
}

// 消费者：`impl Window::a11y_take` 的薄委托（外部调用点见该方法文档）。
/// 产出一笔 a11y 交付（消费 `A11Y` 反向集合与 `full_pending`）。
///
/// 调用方：平台层的 `update_if_active` 闭包（生产）、`App::frame` 的捕获路径（测试）。
pub(crate) fn a11y_take(
    window: &mut Window,
    diagnostics: &mut Vec<crate::a11y::A11yDiagnostic>,
) -> Option<crate::a11y::A11yTreeSnapshot> {
    let Window {
        tree,
        a11y,
        focus,
        scale,
        options,
        ..
    } = window;
    a11y.take(
        tree,
        focus.focused(),
        *scale,
        options.title.as_str(),
        diagnostics,
    )
}

// 消费者：`impl Window::a11y_note_scroll` 的薄委托（外部调用点见该方法文档）。
/// 某个子树内容整体平移（滚动：子孙 bounds 全变 ⇒ 重发整棵子树）。
pub(crate) fn a11y_note_scroll(window: &mut Window, container: crate::node::id::NodeId) {
    window.a11y.invalidate_subtree(container);
}

// 消费者：`impl Window::a11y_note_bind_work` 的薄委托（外部调用点见该方法文档）。
/// 只有 `bind_a11y` 的节点被写信号（没有像素要画，但下帧仍要跑样式 pass）。
pub(crate) fn a11y_note_bind_work(window: &mut Window) {
    window.a11y.note_bind_work();
}

// 消费者：`impl Window::a11y_clear_bind_work` 的薄委托（外部调用点见该方法文档）。
/// 清掉「a11y 绑定待求值」旗标（`refresh_styles` 的入口处）。
pub(crate) fn a11y_clear_bind_work(window: &mut Window) {
    window.a11y.clear_bind_work();
}

// 消费者：`impl Window::set_a11y_capture` 的薄委托（外部调用点见该方法文档）。
/// 打开/关闭 a11y 快照捕获（仅 `test-support` 的 `TestApp` 调用）。
#[cfg(feature = "test-support")]
pub(crate) fn set_a11y_capture(window: &mut Window, on: bool) {
    window.a11y.set_capture(on);
}

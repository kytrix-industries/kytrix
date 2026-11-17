//! P11 a11y：快照交付与捕获（窗口侧入口）。

use super::Window;

// 消费者：`impl Window::a11y_pending` 的薄委托（外部调用点见该方法文档）。
// ── P11：a11y（快照交付 + 捕获） ─────────────────────────────────────────

/// 本轮是否有 a11y 工作（O(1)；平台泵每轮用它过滤 `update_if_active` 调用）。
pub(crate) fn a11y_pending(window: &Window) -> bool {
    window.a11y.pending(&window.tree, window.focus.focused())
}

// 消费者：`impl Window::a11y_focus_pending` 的薄委托。
/// 焦点是否变化且尚未交付。`App::a11y_pending` 用它给焦点变化开一条**不节流**的旁路：
/// 数据类更新仍按 `A11Y_THROTTLE`（100 ms）合并，但 Tab/点击造成的焦点变化在下一轮泵就
/// 交付（0~16 ms），读屏的位置感不再断裂（R13-5）。
pub(crate) fn a11y_focus_pending(window: &Window) -> bool {
    window
        .a11y
        .focus_pending(&window.tree, window.focus.focused())
}

// 消费者：`impl Window::a11y_engine_of` 的薄委托（动作解码）。
/// 稳定 a11y id → 当前引擎句柄（未知 / 已回收 ⇒ `None`，调用方记 `StaleAction`）。
pub(crate) fn a11y_engine_of(
    window: &Window,
    id: crate::a11y::A11yNodeId,
) -> Option<crate::node::id::NodeId> {
    window.a11y.engine_of(id)
}

// 消费者：`impl Window::a11y_stable_id` 的薄委托（动作测试）。
/// 引擎句柄 → 稳定 a11y id（只查不分配）。
#[cfg(test)]
pub(crate) fn a11y_stable_id(
    window: &Window,
    node: crate::node::id::NodeId,
) -> Option<crate::a11y::A11yNodeId> {
    window.a11y.stable_of(node)
}

// 消费者：`impl Window::a11y_force_full` 的薄委托。
/// 强制下一笔交付为全树（测试 oracle 的"全量侧"；`build_update` 的兜底重试也用它）。
pub(crate) fn a11y_force_full(window: &mut Window) {
    window.a11y.force_full();
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
        ime_cursor,
        ..
    } = window;
    // IME 插入点提示（P16）是引擎里**唯一**的 caret 通道：a11y 用它填 `text_selection`，
    // 读屏才能播报"光标在第 N 个字符"（A5）。
    let caret = ime_cursor.map(|request| (request.node, request.byte_index));
    a11y.take(
        tree,
        focus.focused(),
        *scale,
        options.title.as_str(),
        caret,
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

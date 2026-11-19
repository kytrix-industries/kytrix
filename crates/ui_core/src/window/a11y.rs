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

/// 消费端树生命周期代际句柄（E1）：交给平台 adapter 的激活/停用回调 `bump()`。
///
/// 回调在 AccessKit 的线程上执行，只能原子递增 + 唤醒主线程；引擎据此把下一笔升成全树。
pub(crate) fn a11y_consumer_epoch(
    window: &Window,
) -> crate::a11y::adapter::A11yConsumerEpoch {
    window.a11y.consumer_epoch()
}

// 消费者：`impl Window::a11y_consumer_epoch_pending` 的薄委托。
/// 消费端是否重置过而全树尚未交付（`App::a11y_pending` 的不节流旁路，E1）。
pub(crate) fn a11y_consumer_epoch_pending(window: &Window) -> bool {
    window.a11y.consumer_epoch_pending()
}

// 消费者：`impl Window::a11y_backlog_pending` 的薄委托。
/// 是否还有未交付的 a11y 批次（首树/DPI 全量重建的分批未走完；`App::a11y_pending` 的
/// 不节流旁路，E5）。
pub(crate) fn a11y_backlog_pending(window: &Window) -> bool {
    window.a11y.backlog()
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

// 消费者：`impl Window::a11y_note_binding` 的薄委托（外部调用点见该方法文档）。
/// 只有 `bind_a11y` 的节点被写信号：登记待重算（不标 `BIND`/`PAINT`，不产生像素）。
pub(crate) fn a11y_note_binding(window: &mut Window, node: crate::node::id::NodeId) -> bool {
    window.a11y.note_binding_work(node)
}

// 消费者：`impl Window::a11y_binding_work` 的薄委托（外部调用点见该方法文档）。
/// 是否有待重算的 a11y 绑定（帧门禁：a11y-only 写仍要出一帧来投递 TreeUpdate）。
pub(crate) fn a11y_binding_work(window: &Window) -> bool {
    window.a11y.binding_work()
}

// 消费者：`impl Window::a11y_take_bindings` 的薄委托（外部调用点见该方法文档）。
/// 取走本批待重算的 a11y 绑定节点（`refresh_a11y_bindings` 的唯一消费者）。
pub(crate) fn a11y_take_bindings(window: &mut Window) -> Vec<crate::node::id::NodeId> {
    window.a11y.take_bindings()
}

// 消费者：`impl Window::a11y_recycle_bindings` 的薄委托（外部调用点见该方法文档）。
/// 归还复用缓冲（容量保留）。
pub(crate) fn a11y_recycle_bindings(window: &mut Window, buffer: Vec<crate::node::id::NodeId>) {
    window.a11y.recycle_bindings(buffer);
}

// 消费者：`impl Window::a11y_only_frame` 的薄委托（外部调用点见该方法文档）。
/// 本帧是否只剩 a11y 工作（R13-6）：帧的其余阶段（文本/布局/paint/编码）全是空转。
///
/// 判据全部 O(1)（反向脏集合 / 旗标），与节点总数无关。调用点在 `refresh_styles` +
/// `refresh_a11y_bindings` **之后**：此时 `BIND` 已消费，剩下的是 `A11Y` 与待投递的交付。
pub(crate) fn a11y_only_frame(window: &Window) -> bool {
    a11y_pending(window)
        && !window.tree.has_layout_work()
        && !window.tree.has_paint_work()
        && !window.tree.has_bind_work()
        // 帧首的滚动夹紧已经跑过（`App::frame` 在取唤醒位之前调用）；没有布局 ⇒ 视口不会
        // 变，帧尾那次夹紧与本帧无关。damage 一旦出现就说明还有像素要写。
        && !window.full_damage_pending
        && window.pending_damage.is_none()
        // `pending_delta` **不**参与门禁：Idle 帧会保留未发布的 delta（P0-4），它不含本帧
        // 的像素工作，也不被 a11y-only 出口消费 —— 留到下一次真实提交即可。把它算进来会
        // 让快路径在任何 Idle 帧之后被永久饿死。
        // 图集行没上去就不能跳过帧（否则新字形永远不出现；补帧由 `App::frame` 的尾部负责）。
        && !window.atlas_rects_pending()
}

// 消费者：`impl Window::a11y_only_frames` 的薄委托（外部调用点见该方法文档）。
/// 本窗口累计的 a11y-only 帧数（仅 `test-support`：帧统计的断言面）。
#[cfg(feature = "test-support")]
pub(crate) fn a11y_only_frames(window: &Window) -> usize {
    window.a11y.a11y_only_frames()
}

// 消费者：`impl Window::record_a11y_only_frame` 的薄委托（外部调用点见该方法文档）。
/// 记一笔 a11y-only 帧：累计计数 + 让最近一帧的 `FrameRecord` 带上本帧的指标（仅
/// `test-support` 构建有帧记录）。
#[cfg(feature = "test-support")]
pub(crate) fn record_a11y_only_frame(window: &mut Window) {
    window.record = Some(crate::window::record::a11y_only_record(window));
}

// 消费者：`impl Window::set_a11y_capture` 的薄委托（外部调用点见该方法文档）。
/// 打开/关闭 a11y 快照捕获（仅 `test-support` 的 `TestApp` 调用）。
#[cfg(feature = "test-support")]
pub(crate) fn set_a11y_capture(window: &mut Window, on: bool) {
    window.a11y.set_capture(on);
}

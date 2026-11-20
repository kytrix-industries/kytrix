//! owner → `xdg_toplevel.set_parent` 的 **pending 表**（RB-11）：owner 未映射时延后补发。
//!
//! # 协议事实（为什么必须延迟）
//!
//! `xdg-shell.xml` 的 `xdg_toplevel.set_parent`：
//!
//! > Only mapped surfaces can have child surfaces. Setting a parent which is not mapped is
//! > equivalent to setting a null parent. If a surface becomes unmapped, its children's parent
//! > is set to the parent of the now-unmapped surface. ... If the now-unmapped surface becomes
//! > mapped again, its parent-child relationship is not restored.
//!
//! 即：owned 窗口建窗那一刻如果 owner 的 surface 还没映射，`set_parent` 被当成 **null
//! parent**，transient 提示永久丢失（owner 之后映射也不会自动恢复）。启动批 / 同一轮创建
//! owner + owned 时正是这种情况：`event_loop::sync_windows` 按 `WindowId` 升序建原生窗口，
//! owner 的 `wl_surface.commit()` 刚发出、compositor 的首个 `configure` 还没回来。
//!
//! # 「owner 已映射」的判据（协议可观测 + 一条显式耦合）
//!
//! **协议可观测的前提**：owner 的首个 `xdg_surface.configure` 已被 `ack_configure`
//! （[`crate::platform::wayland::configure`] 的映射阶段 —— 协议要求先 ack 才允许 attach
//! buffer 映射），且 attach buffer 的那次 `wl_surface.commit` 已被 compositor 处理。客户端能
//! 拿到的协议证据是**首帧 `wl_callback.done`**（`wl_surface.frame` 的回调由 compositor 在处理
//! 完已提交内容之后发送）。「configure 已到」本身不够：ack 之后 buffer 还没 commit，surface
//! 仍未 mapped（映射的定义就是 attach 了 buffer 且被合成），此时 `set_parent` 依然等价 null
//! parent。
//!
//! **显式耦合（不是协议强制，是设计取舍）**：平台层把「补发时机」绑在**渲染路径**上（`mapped`
//! 只由帧回调置位）。本仓对不可见窗口**根本不请求帧**（`event_loop.rs` 的 `frame_windows` 只含
//! `is_visible && has_renderer` 的窗口），因此：
//!
//! - owner 在生命周期内**从未呈现**（启动即最小化 / 一直被完全遮挡）⇒ pending 永远不会补发，
//!   提示彻底丢失（与修复前的行为等价，不是「晚到」）；
//! - owner 首次呈现之后的补发不会被撤销；owner 之后 unmapped 再 mapped 也不重复补发
//!   （协议：*"If the now-unmapped surface becomes mapped again, its parent-child relationship
//!   is not restored"*）。
//!
//! # 为什么放在 `platform/wayland/`
//!
//! 与 [`crate::platform::wayland::owner`] / [`crate::platform::wayland::scroll`] 同一理由：
//! `platform/linux/**` 只在 Linux 目标编译，开发机（Windows）的 `cargo test` 覆盖不到；pending
//! 表的增删与「何时补发」是纯逻辑（输入是 id + 布尔，输出是待发 id 列表），抽到这里后能在
//! Windows 单测。`xdg_toplevel` 的真实调用留在 `platform/linux/surface.rs`。

use crate::window::WindowId;

/// owned 窗口建窗时 `xdg_toplevel.set_parent` 的落地决定（[`ParentHints::note`] 的返回值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParentAction {
    /// owner 已映射：调用方立刻发 `set_parent`。
    SendNow,
    /// owner 尚未映射：已登记进 pending，等 owner 首次映射后由
    /// [`ParentHints::take_children_of`] 取走补发（现在发等于 null parent，提示会丢）。
    Deferred,
}

/// owner 未映射期间等待补发的 `set_parent` 请求（RB-11）。
///
/// 只存 `WindowId`、不存协议对象：补发时按 id 在原生窗口表里查 toplevel，查不到（窗口已关）
/// 就丢弃该项 —— 不会留下悬空的协议对象，也不会 panic。登记顺序 = 补发顺序（确定且可测）。
#[derive(Default)]
pub(crate) struct ParentHints {
    /// `(child, owner)`：child 等 owner 首次映射后补发 `set_parent(owner)`。
    pending: Vec<(WindowId, WindowId)>,
}

impl ParentHints {
    /// 登记一个 owned 窗口的 owner 提示，并裁决现在发还是等 owner 映射后发。
    ///
    /// `owner_mapped` 必须是**协议可观测**的判据（首帧 `wl_callback.done` 之后为 `true`，见
    /// 模块文档），不得用「object 已创建」这类猜测。
    pub(crate) fn note(
        &mut self,
        child: WindowId,
        owner: WindowId,
        owner_mapped: bool,
    ) -> ParentAction {
        if owner_mapped {
            ParentAction::SendNow
        } else {
            self.pending.push((child, owner));
            ParentAction::Deferred
        }
    }

    /// owner 首次映射 ⇒ 取走所有等它的 child（登记顺序），由调用方逐个补发 `set_parent`。
    ///
    /// 取走即删除 ⇒ **幂等**：同一次映射关系只补发一次，owner 再次「映射」（协议本来也不会
    /// 恢复已断开的 parent 关系）不会重复发。
    pub(crate) fn take_children_of(&mut self, owner: WindowId) -> Vec<WindowId> {
        let mut children = Vec::with_capacity(self.pending.len());
        self.pending.retain(|(child, pending_owner)| {
            if *pending_owner == owner {
                children.push(*child);
                false
            } else {
                true
            }
        });
        children
    }

    /// 窗口销毁（owner 或 child 都有可能）：丢弃相关 pending。
    ///
    /// - owner 先销毁 ⇒ 提示永久失效（协议没有「父销毁」错误，但也没有可补发的对象），
    ///   直接丢弃；child 之后映射也不会再收到 `set_parent`；
    /// - child 先销毁 ⇒ 没有接收者，丢弃。
    pub(crate) fn forget(&mut self, window: WindowId) {
        self.pending
            .retain(|(child, owner)| *child != window && *owner != window);
    }
}

#[cfg(test)]
mod tests {
    use super::{ParentAction, ParentHints};
    use crate::window::WindowId;

    fn window(id: u32) -> WindowId {
        WindowId::for_test(id)
    }

    /// RB-11 核心：owner 尚未映射时**不得当场发** `set_parent`（协议：等价 null parent），
    /// 必须登记 pending。
    ///
    /// 修复前的实现无条件在 `add_window` 里发一次（`note` 等价于永远 `SendNow`）⇒ 本测试红。
    #[test]
    fn an_unmapped_owner_defers_the_hint_until_it_maps() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        assert_eq!(
            hints.note(child, owner, false),
            ParentAction::Deferred,
            "owner 未映射时立刻发 set_parent 等于 null parent，提示会丢"
        );
        assert_eq!(
            hints.take_children_of(owner),
            vec![child],
            "owner 映射后必须把等待中的 child 交回调用方补发"
        );
    }

    /// owner 已映射（运行期新增的对话框）：立刻发，不进 pending。
    #[test]
    fn a_mapped_owner_sends_the_hint_immediately() {
        let mut hints = ParentHints::default();
        assert_eq!(
            hints.note(window(2), window(1), true),
            ParentAction::SendNow,
            "owner 已映射 ⇒ 当场发 set_parent"
        );
        assert!(
            hints.take_children_of(window(1)).is_empty(),
            "已经发过的提示不得再进补发队列（否则会重复 set_parent）"
        );
    }

    /// 补发必须幂等：取走一次之后同一 owner 再「映射」不会重复返回同一 child。
    ///
    /// 变异检查：`take_children_of` 只读不删（`retain` 恒 `true`）⇒ 本测试红。
    #[test]
    fn deferred_hints_are_flushed_exactly_once() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        assert_eq!(hints.note(child, owner, false), ParentAction::Deferred);

        assert_eq!(hints.take_children_of(owner), vec![child]);
        assert!(
            hints.take_children_of(owner).is_empty(),
            "同一次 owner 映射只补发一次，第二次必须为空"
        );
    }

    /// 多个 owned 窗口等同一个 owner：按登记顺序补发（结果确定、可断言）。
    #[test]
    fn hints_are_flushed_in_registration_order() {
        let mut hints = ParentHints::default();
        let owner = window(1);
        for child in [window(3), window(2), window(4)] {
            assert_eq!(hints.note(child, owner, false), ParentAction::Deferred);
        }
        assert_eq!(
            hints.take_children_of(owner),
            vec![window(3), window(2), window(4)],
            "补发顺序 = 登记顺序"
        );
    }

    /// 别的 owner 的 pending 不受影响：一次映射只能取走自己的 child。
    #[test]
    fn flushing_one_owner_leaves_other_owners_pending() {
        let mut hints = ParentHints::default();
        hints.note(window(2), window(1), false);
        hints.note(window(4), window(3), false);

        assert_eq!(hints.take_children_of(window(1)), vec![window(2)]);
        assert_eq!(
            hints.take_children_of(window(3)),
            vec![window(4)],
            "另一个 owner 的等待项必须保留"
        );
    }

    /// owner 先销毁：pending 丢弃（不 panic、不泄漏），之后也不会有补发。
    ///
    /// 变异检查：`forget` 改成 no-op ⇒ 本测试红（并会在真机上对一个已销毁的 toplevel 发
    /// `set_parent`，那是协议错误）。
    #[test]
    fn a_destroyed_owner_drops_its_pending_hints() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        hints.note(child, owner, false);

        hints.forget(owner);
        assert!(
            hints.take_children_of(owner).is_empty(),
            "owner 已关闭 ⇒ 不得再补发 set_parent"
        );
    }

    /// child 先销毁：没有接收者，pending 丢弃。
    #[test]
    fn a_destroyed_child_drops_its_pending_hint() {
        let mut hints = ParentHints::default();
        let child = window(2);
        let owner = window(1);
        hints.note(child, owner, false);

        hints.forget(child);
        assert!(
            hints.take_children_of(owner).is_empty(),
            "child 已关闭 ⇒ 不得再对不存在的 toplevel 发 set_parent"
        );
    }

    /// `forget` 只删相关项：其它窗口的 pending 必须保留。
    #[test]
    fn forgetting_one_window_keeps_unrelated_hints() {
        let mut hints = ParentHints::default();
        hints.note(window(2), window(1), false);
        hints.note(window(4), window(3), false);

        hints.forget(window(2)); // 只关掉第一个 child
        assert!(hints.take_children_of(window(1)).is_empty());
        assert_eq!(
            hints.take_children_of(window(3)),
            vec![window(4)],
            "无关的 owner/child 组合不得被误删"
        );
    }
}
